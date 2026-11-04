//! The file work of a media attempt (W5b, brief §2.2, decisions G1, G5-G8, G16): PREPARE turns the core's file into
//! WhatsApp's ciphertext in the sealed temporary file, and [`SealedSource`] is what the upload reads it through.
//!
//! PREPARE ([`prepare`], blocking: the attempt runs it on the blocking pool, one picture in memory at a time):
//! 1. the core's file is found inside the blob directory only ([`tmp::core_file`]);
//! 2. [`route::look`] says what to read inside it BEFORE the route is known (the route decides the HKDF type, so
//!    it is final before the first byte is encrypted): a still picture up to [`route::PICTURE_LIMIT`] is read whole
//!    and cleaned by [`still`] (G7, G7a); an MP4 is walked ([`mp4::walk`], a second bounded read pass of at most
//!    [`route::MEDIA_LIMIT`]); an Ogg's first [`route::OGG_HEAD`] bytes and an MP3's first [`route::MPEG_HEAD`] are read; everything else is read once;
//! 3. [`route::route`] decides the shape, the upload type and the HKDF type (one value);
//! 4. the bytes to send (the cleaned picture from memory, every other file streamed from the core's blob, untouched)
//!    are encrypted under a FRESH random media key (G5) into a [`TempFile`] (a sealed kourier-blob, G3).
//!
//! Every read of the core's file and of the temporary file goes through [`Halted`]: once the person's stop is
//! accepted ([`Stop::request`]), the app's quit took the attempt ([`Stop::quit`], fix F16) or the attempt has ended
//! another way ([`Halt::end`]) the next `read` fails, so the encryption, the box walk and a blocking POST that is
//! still running end at their next read (upstream cannot cancel a POST: the brief §1.1 item 5). The stop is answered
//! as [`super::stop::stopped`], the quit as [`super::stop::quitting`] (`Transient`: the core sends the message again).
//!
//! UPLOAD ([`send_file`], G12, G16, brief §2.2 "REPORTS" and "THE STALL RULE"): the session's upload of the sealed
//! file, under the person's stop ([`Stop::until_stopped`]: a stop drops the upload's future, whose task the session
//! aborts, and the source's next read fails), with the 250 ms reports ([`Reports`]) and the stall watch. The watch
//! is armed only while a reader of the source is OPEN ([`Watch`]: from `reader_from` until the reader is dropped): no
//! byte read for [`UPLOAD_STALL`] while the body is read, or for [`UPLOAD_TAIL_STALLS`] times as long after its last
//! byte (Signal's rule: what is still in the socket's buffer and the CDN's hashing are counted by nothing), ends
//! the attempt `Transient` with [`STALL_RETRY`]. The body's last byte is found by POSITION (the reader knows the
//! source's length: ureq, which sends a sized body, never reads again after the last byte, so no read answers 0;
//! fix F6), the dropped reader disarms the watch (ureq holds the reader until the answer's head is in). The `w:m`
//! IQ and the resume check are not watched: they are bounded by their own timeouts (the IQ's, the HTTP agent's).
//!
//! What the person's file says (its bytes, name, the picture) never reaches a log: errors carry fixed words.

use std::collections::HashMap;
use std::io::{self, Read};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use kourier_backend_api::{BackendEvent, Envelope};
use kourier_blob::{BlobReader, FileKey};
use kourier_model::{Attachment, BackendError, ErrorKind, LocalMsgId, SendPhase};
use tokio::sync::mpsc;
use wacore::upload::{EncryptedMediaInfo, UploadSource, encrypt_media_streaming, encrypted_len};
use zeroize::Zeroizing;

use super::mp4::{self, Facts};
use super::route::{self, Look, Probe, Route};
use super::stop::Stop;
use super::tmp::{self, TempFile};
use crate::media::{self, MediaPointer, hex};
use crate::session::{Session, Uploaded};

/// The level still pictures are sent at: Signal's level THREE (at most 4096 px, a JPEG of at most 3 MiB at quality
/// 0.75; EXIF, GPS and XMP dropped), one picture policy for every network (G7).
pub(crate) const PICTURE_LEVEL: kourier_media::ScaleLevel = kourier_media::ScaleLevel::THREE;

/// The inline thumbnail's longest edge (`jpegThumbnail`).
pub(crate) const THUMB_EDGE: u32 = 100;

/// The inline thumbnail's JPEG quality (critic L1: always a JPEG, a picture with alpha too).
pub(crate) const THUMB_QUALITY: f32 = 0.5;

/// The largest inline thumbnail sent; a larger one is left out [U7].
pub(crate) const THUMB_MAX: usize = 16 * 1024;

/// The messages whose POST may still run on a blocking thread (fix F5, robustness LOW: upstream cannot cancel a POST, and
/// a CDN connection that stays open but stops taking bytes leaves the thread blocked in a socket write for the whole
/// body budget, ~34 h for 2 GB, holding a blocking-pool thread and a ciphertext-sized sealed temporary file). The reader
/// of the source holds a [`Linger`] while it lives, so the entry goes when the POST's thread drops it. An attempt of a
/// message whose earlier POST still runs answers `Transient{STALL_RETRY}` BEFORE any file work (no prepare, no temporary
/// file, no `w:m` IQ), so a resend adds no second thread and no second file. A known limit: other messages are not
/// limited. One per account (the actor's), cloned into every attempt's [`Files`](super::attempt::Files).
#[derive(Clone, Default)]
pub(crate) struct Lingering(Arc<Mutex<HashMap<LocalMsgId, usize>>>);

impl Lingering {
    fn map(&self) -> MutexGuard<'_, HashMap<LocalMsgId, usize>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether a reader of `local`'s source is still alive (an earlier POST still runs).
    pub(crate) fn holds(&self, local: LocalMsgId) -> bool {
        self.map().contains_key(&local)
    }

    /// A reader of `local`'s source is open until the returned guard goes.
    fn enter(&self, local: LocalMsgId) -> Linger {
        *self.map().entry(local).or_insert(0) += 1;
        Linger {
            set: self.clone(),
            local,
        }
    }
}

/// One open reader of a message's source ([`Lingering::enter`]).
pub(crate) struct Linger {
    set: Lingering,
    local: LocalMsgId,
}

impl Drop for Linger {
    fn drop(&mut self) {
        let mut map = self.set.map();
        if let Some(open) = map.get_mut(&self.local) {
            *open -= 1;
            if *open == 0 {
                map.remove(&self.local);
            }
        }
    }
}

/// The stop of an attempt as its reads see it: the person's stop, or the attempt gone (a stall, a quit, a dropped
/// task: [`Halt::end`]). Cheap to clone; every clone is the same state.
#[derive(Clone)]
pub(crate) struct Halt {
    stop: Stop,
    gone: Arc<AtomicBool>,
    /// The account's lingering POSTs and this attempt's message: the source's readers register there (fix F5).
    linger: Option<(Lingering, LocalMsgId)>,
}

impl Halt {
    /// The reads of an attempt with this stop.
    pub(crate) fn new(stop: Stop) -> Self {
        Self {
            stop,
            gone: Arc::new(AtomicBool::new(false)),
            linger: None,
        }
    }

    /// The same halt, with the readers of the source registered in `set` as `local`'s (fix F5).
    pub(crate) fn lingering(mut self, set: Lingering, local: LocalMsgId) -> Self {
        self.linger = Some((set, local));
        self
    }

    /// A guard for a reader of the source being opened.
    fn linger(&self) -> Option<Linger> {
        self.linger.as_ref().map(|(set, local)| set.enter(*local))
    }

    /// The attempt ended without its reads (its guard): a blocking read still running fails at its next call.
    pub(crate) fn end(&self) {
        self.gone.store(true, Ordering::Release);
    }

    /// A guard that ends the reads when it goes (the attempt's file work, whichever way it ends: a blocking prepare
    /// or a POST still running after its attempt was dropped fails at its next read).
    pub(crate) fn ends_with_guard(&self) -> EndsReads {
        EndsReads(self.clone())
    }

    /// The person's stop of the attempt.
    pub(crate) fn stop(&self) -> &Stop {
        &self.stop
    }

    /// Whether the next read must fail.
    pub(crate) fn halted(&self) -> bool {
        self.stop.is_ended() || self.gone.load(Ordering::Acquire)
    }

    /// What a read or a prepare that found the halt answers: the person's stop ([`super::stop::stopped`]), or, when the
    /// app's quit took the attempt (fix F16), [`super::stop::quitting`] (`Transient`: the core sends the message again).
    pub(crate) fn halted_error(&self) -> BackendError {
        self.stop.ended_error()
    }

    fn error() -> io::Error {
        io::Error::other("the upload was stopped")
    }
}

/// [`Halt::ends_with_guard`]'s guard.
pub(crate) struct EndsReads(Halt);

impl Drop for EndsReads {
    fn drop(&mut self) {
        self.0.end();
    }
}

/// A reader that fails once its [`Halt`] says so, and keeps its position (the bytes read through it, a resume
/// offset's discard included): `sent` gets the MAXIMUM of the positions (G16: monotone across hosts and passes,
/// each of which reads from its own offset again).
pub(crate) struct Halted<R> {
    inner: R,
    halt: Halt,
    pos: u64,
    sent: Option<Arc<AtomicU64>>,
    /// The inner reader failed (not the halt): the core's file does not read.
    inner_failed: bool,
    /// The failure is the machine's moment (see [`machine_moment`], fix F18), not the file's.
    inner_machine: bool,
    /// The temporary file the reader reads, kept until the reader goes (the last holder removes it).
    _file: Option<Arc<TempFile>>,
    /// The stall watch of the upload this reader serves (a reader of the source only).
    watch: Option<Arc<Watch>>,
    /// Where the body ends (the source's length; a reader of the source only, fix F6): the watch moves to its tail
    /// when the position reaches it, because a client that sends a SIZED body (ureq) never reads again after the last
    /// byte, so no read ever answers 0.
    end: Option<u64>,
    /// The message's entry in [`Lingering`] while this reader lives (a reader of the source only; fix F5).
    _linger: Option<Linger>,
}

impl<R: Read> Halted<R> {
    fn new(inner: R, halt: Halt) -> Self {
        Self {
            inner,
            halt,
            pos: 0,
            sent: None,
            inner_failed: false,
            inner_machine: false,
            _file: None,
            watch: None,
            end: None,
            _linger: None,
        }
    }
}

impl<R> Drop for Halted<R> {
    fn drop(&mut self) {
        if let Some(watch) = &self.watch {
            watch.closed();
        }
    }
}

impl<R: Read> Read for Halted<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.halt.halted() {
            return Err(Halt::error());
        }
        match self.inner.read(buf) {
            Ok(n) => {
                self.pos += n as u64;
                if let Some(sent) = &self.sent {
                    sent.fetch_max(self.pos, Ordering::AcqRel);
                }
                if let Some(watch) = &self.watch {
                    watch.read(
                        n,
                        buf.is_empty(),
                        self.end.is_some_and(|end| self.pos >= end),
                    );
                }
                Ok(n)
            }
            Err(e) => {
                self.inner_failed = true;
                self.inner_machine = machine_moment(&e);
                Err(e)
            }
        }
    }
}

/// What a still picture is sent as (G7a: [`still`]).
pub(crate) struct Still {
    /// The bytes to send.
    pub bytes: Zeroizing<Vec<u8>>,
    /// Their MIME type.
    pub mime: String,
    /// Their size in pixels, when known.
    pub dims: Option<(u32, u32)>,
    /// Whether the decoder here opens them (a picture message may carry them, with a thumbnail of that decode);
    /// `false`: a picture this build does not clean (HEIC, AVIF) or one only stripped because it does not decode (an
    /// arithmetic-coded JPEG, a side over 16,384 px), which goes as a document.
    pub opens: bool,
    /// The inline thumbnail (JPEG, at most [`THUMB_MAX`] bytes), of a picture that opens only.
    pub thumbnail: Option<Vec<u8>>,
}

/// What WhatsApp states it cleans (`NetworkDescriptor`'s statement, LOC F14, G7b): every type
/// `kourier_media::clean_still` cleans, up to [`route::PICTURE_LIMIT`]. A larger picture is never read whole and goes
/// as a document, streamed untouched (the core asks before it lets such a picture leave with its location), and so does
/// a GIF (not a cleaned type).
pub(crate) fn stated_cleaning() -> kourier_model::CleanedPictures {
    kourier_model::CleanedPictures {
        max_bytes: Some(route::PICTURE_LIMIT),
        ..kourier_model::CleanedPictures::stills(kourier_media::CLEANED_TYPES)
    }
}

/// THE ONE STILL-PICTURE FUNCTION (G7a, G7b; the reason WhatsApp states [`stated_cleaning`]): every picture
/// [`prepare`] reads whole goes through [`kourier_media::clean_still`], which re-encodes it at [`PICTURE_LEVEL`]
/// without any metadata block as on Signal, strips a JPEG or PNG that does not re-encode (only the orientation kept),
/// and never returns bytes that still carry a location. A type that function does not clean (by its magic: HEIC, AVIF)
/// keeps its bytes and the `mime` and `dims` the core gave it: the core's verdict asked for exactly those (LOC N7). A
/// picture of a cleaned type that cannot be cleaned fails its attachment, with fixed words. Blocking.
pub(crate) fn still(
    original: &[u8],
    mime: &str,
    dims: Option<(u32, u32)>,
) -> Result<Still, BackendError> {
    match kourier_media::clean_still(original, PICTURE_LEVEL) {
        Ok(cleaned) => {
            let decoded = kourier_media::decode(&cleaned.bytes);
            let opens = decoded.is_ok();
            let thumbnail = decoded
                .and_then(|d| d.encode_jpeg(Some(THUMB_EDGE), THUMB_QUALITY))
                .ok()
                .map(|t| t.bytes)
                .filter(|t| t.len() <= THUMB_MAX);
            if !opens {
                tracing::info!("a picture stripped, not re-encoded; sent as a document");
            }
            Ok(Still {
                mime: cleaned.mime.to_owned(),
                dims: Some((cleaned.width, cleaned.height)),
                bytes: Zeroizing::new(cleaned.bytes),
                opens,
                thumbnail,
            })
        }
        Err(kourier_media::NotCleaned::NotMine) => {
            tracing::info!("a picture of a type not cleaned here; sent as a document, as it is");
            Ok(Still {
                bytes: Zeroizing::new(original.to_vec()),
                mime: mime.to_owned(),
                dims,
                opens: false,
                thumbnail: None,
            })
        }
        Err(e @ kourier_media::NotCleaned::Refused) => {
            tracing::warn!(error = %e, "a picture not sent");
            Err(BackendError::new(ErrorKind::InvalidInput, e.to_string()))
        }
    }
}

/// A file ready to upload: WhatsApp's ciphertext in the sealed temporary file, and what the message says of it.
pub(crate) struct Prepared {
    /// The sealed temporary file (the attempt and the upload's reader share it).
    pub file: Arc<TempFile>,
    /// The media key and the hashes. SECRET (the key): never printed.
    pub info: EncryptedMediaInfo,
    /// The ciphertext's length (with its MAC) = `encrypted_len(info.file_length)`.
    pub len: u64,
    /// How the file goes.
    pub route: Route,
    /// The mimetype the message carries.
    pub mime: String,
    /// Width and height (a picture's re-encode, an MP4's video track).
    pub dims: Option<(u32, u32)>,
    /// Whole seconds (an MP4).
    pub seconds: Option<u32>,
    /// The inline thumbnail of a picture.
    pub thumbnail: Option<Vec<u8>>,
}

impl std::fmt::Debug for Prepared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Prepared")
            .field("route", &self.route)
            .field("len", &self.len)
            .finish_non_exhaustive()
    }
}

impl Prepared {
    /// The pointer of the file once uploaded (B10; B11 commits it to the upload record before the build): what
    /// [`media::file_message`] writes the sent message from, a resend and a caption edit again. `captioned`: the
    /// message carries a caption (a document then goes inside its wrapper). SECRET (the key).
    pub(crate) fn pointer(&self, uploaded: &Uploaded, captioned: bool) -> MediaPointer {
        let shape = match self.route.shape {
            route::Shape::Image => "image",
            route::Shape::Video => "video",
            route::Shape::Audio => "audio",
            route::Shape::Document => "document",
        };
        let info = &self.info;
        MediaPointer {
            media_type: shape.to_owned(),
            direct_path: uploaded.direct_path.clone(),
            media_key: hex(&info.media_key),
            file_sha256: Some(hex(&info.file_sha256)),
            file_enc_sha256: Some(hex(&info.file_enc_sha256)),
            file_length: info.file_length,
            media_key_timestamp: Some(uploaded.media_key_timestamp),
            static_url: None,
            thumb_jpeg: self.thumbnail.as_deref().map(hex),
            url: Some(uploaded.url.clone()),
            streaming_sidecar: info.streaming_sidecar.as_deref().map(hex),
            mimetype: Some(self.mime.clone()),
            width: self.dims.map(|(w, _)| w),
            height: self.dims.map(|(_, h)| h),
            seconds: self.seconds,
            shape: Some(
                if captioned && self.route.shape == route::Shape::Document {
                    media::CAPTIONED_DOCUMENT
                } else {
                    shape
                }
                .to_owned(),
            ),
        }
    }
}

#[cfg(test)]
thread_local! {
    /// How many pictures [`prepare`] read whole on this thread (critic H2: a picture over the limit never is).
    pub(crate) static WHOLE_READS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// The core's file does not read (a damaged blob, a wrong key, a missing file): sending it again cannot help.
fn unreadable() -> BackendError {
    BackendError::new(
        ErrorKind::InvalidInput,
        "the file to send could not be read",
    )
}

/// Whether a failed read of the core's file is the MACHINE's moment (out of file descriptors, an I/O error, an
/// interrupted call: the same file may read a moment later) and not the FILE's (fix F18, robustness LOW): a file that is
/// missing, a tag that does not verify (a damaged file or a wrong key), a truncated or a foreign one fails every time.
/// The blob crate's own verdicts come through `std::io` wrapping a `BlobError`; anything else is the operating
/// system's.
fn machine_moment(e: &io::Error) -> bool {
    let the_blobs_verdict = e
        .get_ref()
        .is_some_and(|inner| inner.is::<kourier_blob::BlobError>());
    !the_blobs_verdict && e.kind() != io::ErrorKind::NotFound
}

/// The core's file could not be read just now (fix F18): tried again after the stall floor, never failed for good
/// (the message the person wrote is not lost to a moment of the machine). Logged without a path or a name.
fn busy() -> BackendError {
    tracing::warn!("the core's file could not be read just now; it is tried again");
    BackendError::new(
        ErrorKind::Transient {
            retry_after: Some(STALL_RETRY),
        },
        "the file to send could not be read just now",
    )
}

/// The answer to an open or a whole read of the core's file that failed with `e` (fix F18).
fn unreadable_by(e: &kourier_blob::BlobError) -> BackendError {
    match e {
        kourier_blob::BlobError::Io(io) if machine_moment(io) => busy(),
        _ => unreadable(),
    }
}

/// ONE still picture is decoded and re-encoded at a time in the whole process (fix F17, robustness LOW): [`still`]
/// holds the original (up to the 64 MiB limit), the decoded pixels (a hundred megapixels is 400 MB) and the encode in
/// memory, and a few messages with pictures sent at once would add those up on a desktop that runs other things. The
/// permit is taken BEFORE the blocking prepare starts (under the person's stop, so the X works while a picture waits)
/// and moves into it: it is released when the blocking thread ends, also when its attempt was dropped meanwhile.
pub(crate) static PICTURES: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);

/// The permit [`prepare`] needs for `attachment`: the picture turn for a still picture (waited for), none for any
/// other file (those are streamed, never read whole). Hold it until the prepare has ended.
pub(crate) async fn picture_turn(
    attachment: &Attachment,
) -> Option<tokio::sync::SemaphorePermit<'static>> {
    if !matches!(
        route::look(attachment.kind, &attachment.mime, attachment.size),
        Some(Look::Still)
    ) {
        return None;
    }
    // The semaphore is never closed.
    PICTURES.acquire().await.ok()
}

/// The pictures [`prepare`] decodes right now in the process and the most it ever saw at once (the test seam of fix F17).
#[cfg(test)]
pub(crate) static DECODING: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
#[cfg(test)]
pub(crate) static MOST_DECODING: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Marks a picture as being decoded for the length of its scope; it lingers a little, so that two prepares that are
/// not kept apart overlap (test seam of fix F17).
#[cfg(test)]
struct Decoding;

#[cfg(test)]
impl Decoding {
    fn enter() -> Self {
        let now = DECODING.fetch_add(1, Ordering::SeqCst) + 1;
        MOST_DECODING.fetch_max(now, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(80));
        Self
    }
}

#[cfg(test)]
impl Drop for Decoding {
    fn drop(&mut self) {
        DECODING.fetch_sub(1, Ordering::SeqCst);
    }
}

/// What the encryption read: the cleaned picture in memory, or the core's file streamed.
enum Plain {
    Memory(Zeroizing<Vec<u8>>),
    Blob,
}

/// PREPARE (the module doc): the core's file of `attachment` -> the sealed temporary file in `tmp_dir`. Blocking.
/// `captioned`: the message carries a caption (an audio route then goes as a document: [`route::captioned`]).
pub(crate) fn prepare(
    blob_dir: &Path,
    tmp_dir: &Path,
    attachment: &Attachment,
    captioned: bool,
    halt: &Halt,
) -> Result<Prepared, BackendError> {
    let local = attachment.local.as_ref().ok_or_else(unreadable)?;
    let path = tmp::core_file(blob_dir, &local.path)?;
    let key = FileKey::from_bytes(*local.key.expose());
    let open = || -> Result<Halted<BlobReader<std::fs::File>>, BackendError> {
        if halt.halted() {
            return Err(halt.halted_error());
        }
        let reader = BlobReader::open(&path, &key).map_err(|e| unreadable_by(&e))?;
        Ok(Halted::new(reader, halt.clone()))
    };
    let (kind, size) = (attachment.kind, attachment.size);
    let look = route::look(kind, &attachment.mime, size).ok_or_else(|| {
        BackendError::new(ErrorKind::Unsupported, "this kind of file is not sent here")
    })?;

    let mut mime = attachment.mime.clone();
    let (mut dims, mut seconds, mut thumbnail) = (None, None, None);
    let mut plain = Plain::Blob;
    let probe = match look {
        Look::Nothing(_) => Probe::Nothing,
        Look::Still => {
            #[cfg(test)]
            WHOLE_READS.with(|n| n.set(n.get() + 1));
            #[cfg(test)]
            let _decoding = Decoding::enter();
            if halt.halted() {
                return Err(halt.halted_error());
            }
            let original = kourier_blob::read_all(&path, &key).map_err(|e| unreadable_by(&e))?;
            let cleaned = still(&original, &attachment.mime, attachment.dims);
            drop(original);
            let cleaned = cleaned?;
            let len = cleaned.bytes.len() as u64;
            mime = cleaned.mime;
            dims = cleaned.dims;
            thumbnail = cleaned.thumbnail;
            plain = Plain::Memory(cleaned.bytes);
            Probe::Still(cleaned.opens.then_some(len))
        }
        Look::Mp4 => {
            let mut reader = open()?;
            let facts = match mp4::walk(&mut reader, size) {
                Ok(facts) => facts,
                Err(_) => return Err(read_failed(&reader)),
            };
            if let Some(Facts {
                seconds: s, video, ..
            }) = facts
            {
                seconds = s;
                dims = video.and_then(|v| v.dims);
            }
            Probe::Mp4(facts)
        }
        Look::Ogg => {
            let mut reader = open()?;
            let mut head = Vec::with_capacity(route::OGG_HEAD);
            if (&mut reader)
                .take(route::OGG_HEAD as u64)
                .read_to_end(&mut head)
                .is_err()
            {
                return Err(read_failed(&reader));
            }
            Probe::Ogg {
                opus: route::opus_head(&head),
            }
        }
        Look::Mpeg => {
            let mut reader = open()?;
            let mut head = Vec::with_capacity(route::MPEG_HEAD);
            if (&mut reader)
                .take(route::MPEG_HEAD as u64)
                .read_to_end(&mut head)
                .is_err()
            {
                return Err(read_failed(&reader));
            }
            Probe::Mpeg {
                mp3: route::mpeg_head(&head),
            }
        }
    };
    let route = route::route(kind, &attachment.mime, size, &probe).ok_or_else(|| {
        BackendError::new(ErrorKind::Unsupported, "this kind of file is not sent here")
    })?;
    let route = if captioned {
        route::captioned(route)
    } else {
        route
    };
    let mime = route.mimetype(&mime).to_owned();
    if route.shape != route::Shape::Image {
        thumbnail = None;
    }

    let (file, mut writer) = TempFile::create(tmp_dir)?;
    let file = Arc::new(file);
    let encrypted = match plain {
        Plain::Memory(bytes) => {
            let mut reader = Halted::new(&bytes[..], halt.clone());
            encrypt_media_streaming(&mut reader, &mut writer, route.media_type())
                .map_err(|_| encrypt_failed(&reader))
        }
        Plain::Blob => {
            let mut reader = open()?;
            encrypt_media_streaming(&mut reader, &mut writer, route.media_type())
                .map_err(|_| encrypt_failed(&reader))
        }
    };
    let info = encrypted?;
    let len = TempFile::finish(writer)?;
    let expected = usize::try_from(info.file_length)
        .ok()
        .map(encrypted_len)
        .and_then(|n| u64::try_from(n).ok());
    if expected != Some(len) {
        return Err(BackendError::internal(
            "the encrypted file has an unexpected length",
        ));
    }
    Ok(Prepared {
        file,
        info,
        len,
        route,
        mime,
        dims,
        seconds,
        thumbnail,
    })
}

/// A read of the core's file failed: the stop, the machine's moment (tried again), or the file itself.
fn read_failed<R>(reader: &Halted<R>) -> BackendError {
    if reader.halt.halted() && !reader.inner_failed {
        reader.halt.halted_error()
    } else if reader.inner_machine {
        busy()
    } else {
        unreadable()
    }
}

/// The encryption failed: the stop, the core's file (the machine's moment or the file itself), or the temporary file's
/// write (the disk; transient, after the stall floor: [`tmp::unwritable`]).
fn encrypt_failed<R>(reader: &Halted<R>) -> BackendError {
    if reader.halt.halted() && !reader.inner_failed {
        reader.halt.halted_error()
    } else if reader.inner_failed {
        read_failed(reader)
    } else {
        tracing::warn!("the temporary upload file could not be written");
        tmp::unwritable()
    }
}

/// No reader of the source is open (before the first `reader_from`, between hosts): not watched.
const IDLE: u8 = 0;
/// A reader is open and has not reached the end of the body.
const READING: u8 = 1;
/// A reader read the body's last byte (the answer is awaited).
const AT_END: u8 = 2;

/// What the stall watch sees of the source's readers: how many bytes they read in all (every read counts, a second
/// host's re-read too: the watch asks whether bytes MOVE, not how far the file got) and where the body stands.
#[derive(Default)]
pub(crate) struct Watch {
    moved: AtomicU64,
    phase: AtomicU8,
}

impl Watch {
    fn opened(&self) {
        self.phase.store(READING, Ordering::Release);
    }

    /// A read gave `n` bytes. The body ends when the reader's absolute position reached the source's length
    /// (`at_end`: the offset of a resumed host included; fix F6) or when a read of a non-empty buffer answered 0
    /// (a client that reads to EOF).
    fn read(&self, n: usize, asked_nothing: bool, at_end: bool) {
        if n > 0 {
            self.moved.fetch_add(n as u64, Ordering::AcqRel);
        }
        if at_end || (n == 0 && !asked_nothing) {
            let _ =
                self.phase
                    .compare_exchange(READING, AT_END, Ordering::AcqRel, Ordering::Acquire);
        }
    }

    /// A reader went (a host failed, or the request has its answer's head): nothing is watched until the next
    /// `reader_from`. From the tail too (fix F6): ureq holds the reader until `send` returns, so the answer's head
    /// is still under the tail rule, and the next host's resume check and the 401's `w:m` IQ are bounded by their
    /// own timeouts, not by a stall.
    fn closed(&self) {
        self.phase.store(IDLE, Ordering::Release);
    }

    /// How long no byte may move now (`None`: not watched).
    fn allowed(&self) -> Option<Duration> {
        match self.phase.load(Ordering::Acquire) {
            READING => Some(UPLOAD_STALL),
            AT_END => Some(UPLOAD_STALL * UPLOAD_TAIL_STALLS),
            _ => None,
        }
    }
}

/// What the upload reads: the sealed temporary file, from any offset, through [`Halted`].
pub(crate) struct SealedSource {
    file: Arc<TempFile>,
    len: u64,
    halt: Halt,
    sent: Arc<AtomicU64>,
    watch: Arc<Watch>,
}

impl SealedSource {
    /// The source of a prepared file.
    pub(crate) fn new(prepared: &Prepared, halt: Halt) -> Self {
        Self {
            file: Arc::clone(&prepared.file),
            len: prepared.len,
            halt,
            sent: Arc::new(AtomicU64::new(0)),
            watch: Arc::new(Watch::default()),
        }
    }

    /// The furthest position any reader of this source reached (ciphertext bytes; G16).
    pub(crate) fn sent(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.sent)
    }

    /// What the stall watch sees of this source's readers.
    pub(crate) fn watch(&self) -> Arc<Watch> {
        Arc::clone(&self.watch)
    }
}

impl UploadSource for SealedSource {
    fn len(&self) -> u64 {
        self.len
    }

    /// The sealed file has no `Seek`: a resume offset (only from 5 MiB, only on a second host) is reached by
    /// reading and discarding, through the halt too.
    fn reader_from(&self, offset: u64) -> io::Result<Box<dyn Read + Send>> {
        if self.halt.halted() {
            return Err(Halt::error());
        }
        let mut reader = Halted::new(self.file.reader()?, self.halt.clone());
        reader.sent = Some(Arc::clone(&self.sent));
        reader.end = Some(self.len);
        reader._file = Some(Arc::clone(&self.file));
        reader._linger = self.halt.linger();
        self.watch.opened();
        reader.watch = Some(Arc::clone(&self.watch));
        let skipped = io::copy(&mut (&mut reader).take(offset), &mut io::sink())?;
        if skipped != offset {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the resume offset is past the file",
            ));
        }
        Ok(Box::new(reader))
    }
}

/// How often the upload is reported (rule 6 of the upload-progress contract; Signal's ticker).
pub(crate) const TICK: Duration = Duration::from_millis(250);

/// No byte of the body read for this long = a stalled upload (Signal's number).
pub(crate) const UPLOAD_STALL: Duration = Duration::from_secs(60);

/// After the body's last byte the answer may take this many stalls (Signal's tail rule, critic H1: what is still in
/// the socket's send buffer and the CDN's hashing of the body are counted by no read).
pub(crate) const UPLOAD_TAIL_STALLS: u32 = 5;

/// A stalled upload is tried again by the core no sooner than this (critic H1: not at once, in a loop of prepare and
/// upload on a slow uplink).
pub(crate) const STALL_RETRY: Duration = Duration::from_secs(30);

/// The reports of ONE file's attempt (the upload-progress contract, rules 6 and 10): ephemeral events of the
/// message; the ticks may be dropped when the queue is full, the first and the last reports are awaited.
pub(crate) struct Reports {
    events: mpsc::Sender<Envelope>,
    local: LocalMsgId,
}

impl Reports {
    pub(crate) fn new(events: mpsc::Sender<Envelope>, local: LocalMsgId) -> Self {
        Self { events, local }
    }

    /// `Preparing` (the X from the first moment of the file work).
    pub(crate) async fn preparing(&self) {
        let _ = self
            .events
            .send(Envelope::ephemeral(BackendEvent::TransmitProgress {
                local: self.local,
                progress: 0.0,
                phase: Some(SendPhase::Preparing),
            }))
            .await;
    }

    /// One tick: `done` of `total` plaintext bytes (never `total`: that is [`full`](Self::full)'s).
    fn tick(&self, done: u64, total: u64) {
        let _ = self
            .events
            .try_send(Envelope::ephemeral(BackendEvent::AttachmentProgress {
                local: self.local,
                index: 0,
                done,
                total,
            }));
        #[allow(clippy::cast_precision_loss, reason = "a share for a progress ring")]
        let share = (done as f64 / total as f64).min(0.99) as f32;
        let _ = self
            .events
            .try_send(Envelope::ephemeral(BackendEvent::TransmitProgress {
                local: self.local,
                progress: share,
                phase: Some(SendPhase::Uploading),
            }));
    }

    /// The file is up (or its upload was reused): the file whole, then the uploads' end (`None`: no 100 % ring
    /// stays through the send).
    pub(crate) async fn full(&self, total: u64) {
        let _ = self
            .events
            .send(Envelope::ephemeral(BackendEvent::AttachmentProgress {
                local: self.local,
                index: 0,
                done: total,
                total,
            }))
            .await;
        let _ = self
            .events
            .send(Envelope::ephemeral(BackendEvent::TransmitProgress {
                local: self.local,
                progress: 1.0,
                phase: None,
            }))
            .await;
    }
}

/// The plaintext bytes reported for `sent` of `len` ciphertext bytes: proportional, at most `total - 1` (the file is
/// whole only when the CDN answered).
fn reported(sent: u64, len: u64, total: u64) -> u64 {
    if len == 0 || total == 0 {
        return 0;
    }
    let share = u128::from(sent.min(len)) * u128::from(total) / u128::from(len);
    u64::try_from(share).unwrap_or(u64::MAX).min(total - 1)
}

/// A copy of the encryption's facts for the session (upstream's type is not `Clone`).
fn info_of(info: &EncryptedMediaInfo) -> EncryptedMediaInfo {
    EncryptedMediaInfo {
        media_key: info.media_key,
        file_sha256: info.file_sha256,
        file_enc_sha256: info.file_enc_sha256,
        file_length: info.file_length,
        streaming_sidecar: info.streaming_sidecar.clone(),
    }
}

/// Ends the attempt's reads when the upload ends any way (a stall, a stop, an error, a dropped attempt): a blocking
/// POST still running fails at its next read.
struct EndReads<'a>(&'a Halt);

impl Drop for EndReads<'_> {
    fn drop(&mut self) {
        self.0.end();
    }
}

/// UPLOAD (the module doc): the prepared file through `session`, reported to `reports`, under the stop and the stall
/// watch. `Ok` = the CDN answered and the file was reported whole; a stop accepted meanwhile answers
/// [`super::stop::stopped`] (the quit: [`super::stop::quitting`]) whatever the upload's own error was.
pub(crate) async fn send_file(
    session: &dyn Session,
    prepared: &Prepared,
    halt: &Halt,
    reports: &Reports,
) -> Result<Uploaded, BackendError> {
    let _end = EndReads(halt);
    let source = SealedSource::new(prepared, halt.clone());
    let (sent, watch) = (source.sent(), source.watch());
    let (len, total) = (prepared.len, prepared.info.file_length);
    let upload = session.upload(source, info_of(&prepared.info), prepared.route.shape);
    let watched = async {
        tokio::pin!(upload);
        let mut ticker = tokio::time::interval(TICK);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut shown = 0;
        let (mut last_moved, mut last_phase) = (0, IDLE);
        let mut since = tokio::time::Instant::now();
        loop {
            tokio::select! {
                biased;
                answer = &mut upload => return answer.map_err(super::errors::upload_error),
                _ = ticker.tick() => {}
            }
            let done = reported(sent.load(Ordering::Acquire), len, total);
            if done > shown {
                shown = done;
                reports.tick(done, total);
            }
            let (moved, phase) = (
                watch.moved.load(Ordering::Acquire),
                watch.phase.load(Ordering::Acquire),
            );
            if moved != last_moved || phase != last_phase {
                (last_moved, last_phase) = (moved, phase);
                since = tokio::time::Instant::now();
            } else if watch
                .allowed()
                .is_some_and(|allowed| since.elapsed() >= allowed)
            {
                tracing::warn!("an upload stalled");
                return Err(BackendError::new(
                    ErrorKind::Transient {
                        retry_after: Some(STALL_RETRY),
                    },
                    "the upload stalled",
                ));
            }
        }
    };
    match halt.stop().until_stopped(watched).await {
        Ok(uploaded) => {
            reports.full(total).await;
            Ok(uploaded)
        }
        Err(_) if halt.stop().is_ended() => Err(halt.halted_error()),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests;
