//! The media handle table (§8.6): attachment id → encrypted file, key and type. The core fills
//! it whenever it answers a timeline query, so the UI's image provider resolves
//! `image://kourier/{att,thumb,blurhash}/<attachment id>` without touching `core.db` (image loads
//! never queue behind timeline queries). A miss (an evicted handle) costs one lookup on the third
//! read connection ([`MediaHandles::resolve`]). Entries are forgotten when their attachment
//! changes; keys are zeroized when an entry is dropped.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use kourier_model::{AccountId, AttachmentId, AttachmentKind, AttachmentState, BlobKey};
use tokio::sync::{mpsc, oneshot};
use zeroize::Zeroizing;

use crate::error::{CoreError, Result};
use crate::facade::Internal;

/// Entries the table keeps (least recently used go first).
pub const MEDIA_HANDLES: usize = 4096;

/// One encrypted file of an attachment.
#[derive(Clone, PartialEq, Eq)]
pub struct MediaBlob {
    /// Absolute path of the `kourier-blob` file.
    pub path: PathBuf,
    /// Its key.
    pub key: BlobKey,
    /// MIME type of the plaintext, when known.
    pub mime: Option<String>,
}

impl std::fmt::Debug for MediaBlob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MediaBlob")
            .field("mime", &self.mime)
            .finish_non_exhaustive()
    }
}

/// What the UI's image provider needs about one attachment.
#[derive(Clone, PartialEq, Eq)]
pub struct MediaEntry {
    /// The attachment.
    pub attachment: AttachmentId,
    /// Its account.
    pub account: AccountId,
    /// Kind.
    pub kind: AttachmentKind,
    /// MIME type.
    pub mime: Option<String>,
    /// Download state when the entry was made.
    pub state: AttachmentState,
    /// The full file, once downloaded.
    pub file: Option<MediaBlob>,
    /// The thumbnail, once derived.
    pub thumb: Option<MediaBlob>,
    /// Blurhash placeholder.
    pub blurhash: Option<String>,
    /// Display dimensions.
    pub dims: Option<(u32, u32)>,
}

kourier_model::redacting_debug!(MediaEntry {
    attachment, account, kind, mime, state, file, thumb, dims
} redact { blurhash });

/// Which file of an entry to read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MediaVariant {
    /// The attachment itself.
    Full,
    /// Its thumbnail (falls back to the full file when there is none). The timeline does not
    /// ask for this one: it asks [`MediaEntry::timeline_blob`] for the size it draws.
    Thumb,
}

/// Pixels above which a file that is not a JPEG is not decoded for the timeline when its
/// thumbnail exists: Qt decodes a PNG, WebP or BMP whole before it scales it (a 4096 × 4096
/// picture is 64 MiB of pixels on each of the provider's three workers), while a JPEG is
/// decoded at a fraction of its size. Signal's own levels stay at or below 4096 px
/// (`kourier-media`'s `scale.rs`), so a picture of its senders is never above this.
///
/// Whether the file is a JPEG is judged by the image provider from the file's bytes
/// (`Codec::sniff`), never from the sender's MIME type (a PNG called `image/jpeg` is a PNG):
/// [`MediaEntry::exceeds_timeline_pixels`] says whether the limit applies.
pub const MAX_TIMELINE_PIXELS: u64 = 4096 * 4096;

impl MediaEntry {
    /// The blob for `variant`, if there is one.
    pub fn blob(&self, variant: MediaVariant) -> Option<&MediaBlob> {
        match variant {
            MediaVariant::Full => self.file.as_ref(),
            MediaVariant::Thumb => self.thumb.as_ref().or(self.file.as_ref()),
        }
    }

    /// Whether the picture has more than [`MAX_TIMELINE_PIXELS`] pixels (false when its
    /// dimensions are not known).
    pub fn exceeds_timeline_pixels(&self) -> bool {
        self.dims
            .is_some_and(|(w, h)| u64::from(w) * u64::from(h) > MAX_TIMELINE_PIXELS)
    }

    /// The blob the timeline draws for a picture that is shown `want` pixels wide and high
    /// (the physical size of the item: the request of the image provider; a side of 0 is
    /// unset): the FILE whenever it is here and the 400 px thumbnail
    /// ([`kourier_media::THUMBNAIL_EDGE`]) cannot cover that size, the thumbnail when it
    /// covers it or while there is no file. Android's rule (`Attachment.displayUri` is the
    /// file's URI when it has one and the thumbnail's until then; its `ThumbnailView` then
    /// decodes the file down to the view, never up): the thumbnail only saves work, it is
    /// never what the user sees stretched.
    ///
    /// The thumbnail keeps `have = min(1, 400 / longest edge)` of the picture's size (the
    /// same fit as `kourier_media`'s `fit`), the picture is drawn to COVER `want` (the
    /// provider's and `PreserveAspectCrop`'s rule), so it needs `need = max(want.w / w,
    /// want.h / h)` of it. The thumbnail does when `need <= have`; compared in integers, so
    /// no rounding decides it. The thumbnail also serves, whatever the size:
    /// - when the dimensions are not known or nothing is asked for (a request without a size
    ///   decodes whole, which is what the viewer asks for with `Full`);
    /// - when the longest edge is not above 400: the file is no larger than the thumbnail.
    ///
    /// The one other case is the provider's, because it needs the file's bytes: a file that
    /// turns out not to be a JPEG while [`exceeds_timeline_pixels`](Self::exceeds_timeline_pixels)
    /// is not drawn from the file but from the thumbnail.
    ///
    /// An entry without a thumbnail (not derived yet, or a format `kourier-media` cannot
    /// decode) gives the file, as [`MediaEntry::blob`] does.
    pub fn timeline_blob(&self, want: (u32, u32)) -> Option<&MediaBlob> {
        let (Some(file), Some(thumb)) = (self.file.as_ref(), self.thumb.as_ref()) else {
            return self.blob(MediaVariant::Thumb);
        };
        let Some((w, h)) = self.dims.filter(|(w, h)| *w > 0 && *h > 0) else {
            return Some(thumb);
        };
        let (w, h) = (u64::from(w), u64::from(h));
        let longest = w.max(h);
        let edge = u64::from(kourier_media::THUMBNAIL_EDGE);
        if longest <= edge {
            return Some(thumb);
        }
        // want.w / w <= edge / longest  <=>  want.w * longest <= edge * w (a side of 0 asks
        // for nothing and always fits: nothing asked at all is the thumbnail).
        let covers =
            u64::from(want.0) * longest <= edge * w && u64::from(want.1) * longest <= edge * h;
        Some(if covers { thumb } else { file })
    }
}

#[derive(Default)]
struct Lru {
    entries: HashMap<AttachmentId, (MediaEntry, u64)>,
    order: BTreeMap<u64, AttachmentId>,
    tick: u64,
}

impl Lru {
    fn touch(&mut self, id: AttachmentId) -> Option<MediaEntry> {
        self.tick += 1;
        let tick = self.tick;
        let (entry, at) = self.entries.get_mut(&id)?;
        self.order.remove(at);
        *at = tick;
        self.order.insert(tick, id);
        Some(entry.clone())
    }

    fn insert(&mut self, entry: MediaEntry) {
        self.tick += 1;
        let id = entry.attachment;
        if let Some((_, at)) = self.entries.insert(id, (entry, self.tick)) {
            self.order.remove(&at);
        }
        self.order.insert(self.tick, id);
        while self.entries.len() > MEDIA_HANDLES {
            let Some((_, oldest)) = self.order.pop_first() else {
                break;
            };
            self.entries.remove(&oldest);
        }
    }

    fn remove(&mut self, id: AttachmentId) {
        if let Some((_, at)) = self.entries.remove(&id) {
            self.order.remove(&at);
        }
    }
}

/// The media handle table: cheap to clone, safe to use from any thread (image-provider workers
/// included). Get it with [`CoreHandle::media`](crate::facade::CoreHandle::media).
#[derive(Clone)]
pub struct MediaHandles {
    lru: Arc<Mutex<Lru>>,
    commands: mpsc::UnboundedSender<Internal>,
    /// Thumbnails the core still has to derive and publish ([`deriving`](Self::deriving)).
    deriving: Arc<AtomicUsize>,
    /// `false` while [`hold_derives`](Self::hold_derives) holds the derive tasks.
    derive_open: Arc<tokio::sync::watch::Sender<bool>>,
    /// UI events sent by a publish with derive jobs, and how many of them went out while fewer
    /// jobs were counted than that publish has ([`reserve_probe`](Self::reserve_probe)).
    reserve_probe: Arc<(AtomicUsize, AtomicUsize)>,
}

impl std::fmt::Debug for MediaHandles {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MediaHandles")
            .field("len", &self.len())
            .finish()
    }
}

impl MediaHandles {
    pub(crate) fn new(commands: mpsc::UnboundedSender<Internal>) -> Self {
        Self {
            lru: Arc::new(Mutex::new(Lru::default())),
            commands,
            deriving: Arc::new(AtomicUsize::new(0)),
            derive_open: Arc::new(tokio::sync::watch::channel(true).0),
            reserve_probe: Arc::new((AtomicUsize::new(0), AtomicUsize::new(0))),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Lru> {
        self.lru.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The entry of `attachment`, if the table has it (never touches a database).
    pub fn get(&self, attachment: AttachmentId) -> Option<MediaEntry> {
        self.lock().touch(attachment)
    }

    /// The entry of `attachment`, looked up on the media read connection when the table does
    /// not have it (and then kept). `None` when the attachment does not exist.
    pub async fn resolve(&self, attachment: AttachmentId) -> Result<Option<MediaEntry>> {
        if let Some(e) = self.get(attachment) {
            return Ok(Some(e));
        }
        let (tx, rx) = oneshot::channel();
        self.commands
            .send(Internal::Media(attachment, tx))
            .map_err(|_| CoreError::Stopped)?;
        rx.await.map_err(|_| CoreError::Stopped)?
    }

    /// [`resolve`](Self::resolve) for a worker thread outside the async runtime (the image
    /// provider's `QRunnable`, which may not `block_on`): waits for the lookup on the media read
    /// connection. Refused with [`CoreError::InvalidState`] on a runtime thread, where it would
    /// stall the runtime (use `resolve` there).
    pub fn resolve_blocking(&self, attachment: AttachmentId) -> Result<Option<MediaEntry>> {
        if let Some(e) = self.get(attachment) {
            return Ok(Some(e));
        }
        if tokio::runtime::Handle::try_current().is_ok() {
            return Err(CoreError::InvalidState(
                "resolve_blocking called on a runtime thread",
            ));
        }
        let (tx, rx) = oneshot::channel();
        self.commands
            .send(Internal::Media(attachment, tx))
            .map_err(|_| CoreError::Stopped)?;
        rx.blocking_recv().map_err(|_| CoreError::Stopped)?
    }

    /// Decrypts `blob` into memory (zeroized on drop), verifying every chunk. Blocking: call it
    /// from a worker thread (the image provider's `QRunnable`), never from the Qt thread.
    pub fn read(&self, blob: &MediaBlob) -> Result<Zeroizing<Vec<u8>>> {
        let key = kourier_blob::FileKey::from_bytes(*blob.key.expose());
        kourier_blob::read_all(&blob.path, &key).map_err(CoreError::Blob)
    }

    /// RGBA pixels (`width` × `height`, each clamped to 1…256) of a blurhash placeholder.
    pub fn placeholder(&self, blurhash: &str, width: u32, height: u32) -> Result<Vec<u8>> {
        kourier_media::blurhash_rgba(blurhash, width, height)
            .map_err(|_| CoreError::InvalidInput("malformed blurhash"))
    }

    /// Thumbnails the core has yet to derive and publish: the derive jobs of every `Changes`
    /// already published to the UI (or about to be) whose results are not in the timeline yet.
    /// A picture that was downloaded changes its URLs once more when its thumbnail is recorded
    /// (the blurhash and the dimensions come with it), so a view that wants the final
    /// picture waits for 0 (`--screenshot` runs: "Stable screenshots", architecture §13).
    ///
    /// A job is counted from BEFORE the UI event that says its attachment is local until AFTER
    /// the events of the group that records its result were SENT (`Shared::emit`), so a reader
    /// that has seen the first event never sees 0 before the second was sent. It is 0 then,
    /// not when the UI has applied them: the events can still be on their way to the
    /// front-end (`CoreHandle::events_unseen`, its queue) and the models' reads they cause can
    /// be in flight (`UiSource::queries_in_flight`); a view that wants the final picture adds
    /// those (`App::screenshot_holds`).
    pub fn deriving(&self) -> usize {
        self.deriving.load(Ordering::SeqCst)
    }

    /// Called at every UI event of a publish that has `jobs` derive jobs: they must be counted
    /// ([`reserve`](Self::reserve)) before the first event goes out, or a reader that has seen
    /// the event sees 0 ([`reserve_probe`](Self::reserve_probe) tells).
    pub(crate) fn check_reserved(&self, jobs: usize) {
        if jobs == 0 {
            return;
        }
        self.reserve_probe.0.fetch_add(1, Ordering::SeqCst);
        if self.deriving() < jobs {
            self.reserve_probe.1.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// For tests: (the UI events sent by publishes with derive jobs, how many of them went out
    /// before their publish's jobs were counted). The second is 0 while the count comes first.
    #[doc(hidden)]
    pub fn reserve_probe(&self) -> (usize, usize) {
        (
            self.reserve_probe.0.load(Ordering::SeqCst),
            self.reserve_probe.1.load(Ordering::SeqCst),
        )
    }

    /// Holds every derive task before its next job until [`release_derives`](Self::release_derives):
    /// a test that needs the time between "downloaded" and "thumbnail published".
    pub fn hold_derives(&self) {
        self.derive_open.send_replace(false);
    }

    /// Lets the held derive tasks go on.
    pub fn release_derives(&self) {
        self.derive_open.send_replace(true);
    }

    /// The gate of [`hold_derives`](Self::hold_derives).
    pub(crate) fn derives_open(&self) -> tokio::sync::watch::Receiver<bool> {
        self.derive_open.subscribe()
    }

    /// Counts `jobs` derive jobs from now until the returned hold releases them. Reserve BEFORE
    /// the UI events of the `Changes` the jobs came with, hand the hold to
    /// [`derive::spawn`](crate::attachments::derive::spawn).
    pub(crate) fn reserve(&self, jobs: usize) -> DeriveHold {
        self.deriving.fetch_add(jobs, Ordering::SeqCst);
        DeriveHold {
            counter: self.deriving.clone(),
            held: jobs,
        }
    }

    /// Number of entries.
    pub fn len(&self) -> usize {
        self.lock().entries.len()
    }

    /// Whether the table is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub(crate) fn insert(&self, entry: MediaEntry) {
        self.lock().insert(entry);
    }

    pub(crate) fn forget(&self, ids: impl IntoIterator<Item = AttachmentId>) {
        let mut lru = self.lock();
        for id in ids {
            lru.remove(id);
        }
    }
}

/// Derive jobs counted in [`MediaHandles::deriving`]; they stay counted until released, one by
/// one ([`release`](Self::release)) or all at once when the hold is dropped (the task ended,
/// returned early or was cancelled), so the count cannot stay above 0 for a job that is gone.
#[derive(Debug)]
pub(crate) struct DeriveHold {
    counter: Arc<AtomicUsize>,
    held: usize,
}

impl DeriveHold {
    /// Stops counting `jobs` of the held ones (at most what is still held).
    pub(crate) fn release(&mut self, jobs: usize) {
        let jobs = jobs.min(self.held);
        self.held -= jobs;
        self.counter.fetch_sub(jobs, Ordering::SeqCst);
    }
}

impl Drop for DeriveHold {
    fn drop(&mut self) {
        self.release(self.held);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: i64) -> MediaEntry {
        MediaEntry {
            attachment: AttachmentId(id),
            account: AccountId(1),
            kind: AttachmentKind::Image,
            mime: Some("image/jpeg".into()),
            state: AttachmentState::Local,
            file: Some(MediaBlob {
                path: PathBuf::from("/x"),
                key: BlobKey::new([7; 32]),
                mime: None,
            }),
            thumb: None,
            blurhash: Some("LKO2?U%2Tw=w]~RBVZRi};RPxuwH".into()),
            dims: Some((4, 3)),
        }
    }

    #[test]
    fn least_recently_used_entries_go_first() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let t = MediaHandles::new(tx);
        for i in 0..MEDIA_HANDLES as i64 {
            t.insert(entry(i));
        }
        // Touch the oldest so it survives the next insert.
        assert!(t.get(AttachmentId(0)).is_some());
        t.insert(entry(-1));
        assert_eq!(t.len(), MEDIA_HANDLES);
        assert!(t.get(AttachmentId(0)).is_some());
        assert!(
            t.get(AttachmentId(1)).is_none(),
            "the least recently used went"
        );
        t.forget([AttachmentId(0), AttachmentId(-1)]);
        assert!(t.get(AttachmentId(0)).is_none());
        assert_eq!(t.len(), MEDIA_HANDLES - 2);
        let s = format!("{:?}", entry(1));
        assert!(!s.contains("7, 7") && !s.contains("LKO2"), "{s}");
        assert_eq!(
            entry(1).blob(MediaVariant::Thumb),
            entry(1).file.as_ref(),
            "a missing thumbnail falls back to the file"
        );
        assert!(t.placeholder("LKO2?U%2Tw=w]~RBVZRi};RPxuwH", 8, 6).is_ok());
        assert!(t.placeholder("x", 8, 6).is_err());
    }

    /// A picture of `w` × `h` with a file and a thumbnail, the way a download leaves it.
    fn picture(w: u32, h: u32, mime: &str) -> MediaEntry {
        let blob = |path: &str| MediaBlob {
            path: PathBuf::from(path),
            key: BlobKey::new([7; 32]),
            mime: Some(mime.to_owned()),
        };
        MediaEntry {
            file: Some(blob("/file")),
            thumb: Some(blob("/thumb")),
            mime: Some(mime.to_owned()),
            dims: Some((w, h)),
            ..entry(1)
        }
    }

    /// What `timeline_blob` serves for `entry` asked at `want`: `"file"`, `"thumb"` or `"none"`.
    fn served(entry: &MediaEntry, want: (u32, u32)) -> &'static str {
        match entry
            .timeline_blob(want)
            .map(|b| b.path.to_str().unwrap_or(""))
        {
            Some("/file") => "file",
            Some("/thumb") => "thumb",
            _ => "none",
        }
    }

    /// The physical size a single picture of `ratio` (height over width) is asked at on a display
    /// of `scale` (`ImageBlock.qml`: 288 logical pixels wide, the ratio clamped to 0.4 … 1.6, both
    /// sides rounded up to physical pixels).
    fn asked(ratio: f64, scale: f64) -> (u32, u32) {
        let ratio = ratio.clamp(0.4, 1.6);
        let wide = (288.0 * scale).ceil() as u32;
        let high = (288.0 * ratio * scale).ceil() as u32;
        (wide, high)
    }

    #[test]
    fn the_timeline_draws_the_file_where_the_thumbnail_cannot_cover_the_size() {
        // A landscape photo at scale 1: 288 × 216 is covered by 400 × 300.
        let landscape = picture(4000, 3000, "image/jpeg");
        assert_eq!(served(&landscape, (288, 216)), "thumb");
        // A phone photo in portrait at scale 2: 576 × 768 asks more than 300 × 400 has.
        let portrait = picture(3000, 4000, "image/jpeg");
        assert_eq!(served(&portrait, (576, 768)), "file");
        // A phone screenshot at scale 1: 288 × 461 against 185 × 400.
        let screenshot = picture(1080, 2340, "image/jpeg");
        assert_eq!(served(&screenshot, (288, 461)), "file");
        // Landscape 4:3 at scale 2 is 576 × 432 against 400 × 300: the file as well.
        assert_eq!(served(&landscape, (576, 432)), "file");
        // The same pictures, small, as an album tile or a quote draws them: the thumbnail.
        assert_eq!(served(&portrait, (144, 144)), "thumb");
        assert_eq!(served(&screenshot, (96, 96)), "thumb");
        assert_eq!(served(&landscape, (96, 96)), "thumb");
    }

    #[test]
    fn the_users_display_at_one_and_a_quarter() {
        // devicePixelRatio 1.25: a single picture 288 logical pixels wide is asked at 360.
        let scale = 1.25;
        let landscape = asked(0.75, scale);
        let portrait = asked(4.0 / 3.0, scale);
        let screenshot = asked(2340.0 / 1080.0, scale);
        assert_eq!(landscape.0, 360);
        assert_eq!((portrait.0, screenshot.0), (360, 360));
        assert_eq!((landscape.1, portrait.1, screenshot.1), (270, 480, 576));
        // A portrait 3:4 and a phone screenshot get the file, a landscape 4:3 the thumbnail.
        assert_eq!(served(&picture(3000, 4000, "image/jpeg"), portrait), "file");
        assert_eq!(served(&picture(1500, 2000, "image/jpeg"), portrait), "file");
        assert_eq!(
            served(&picture(1080, 2340, "image/jpeg"), screenshot),
            "file"
        );
        assert_eq!(
            served(&picture(4000, 3000, "image/jpeg"), landscape),
            "thumb"
        );
        assert_eq!(
            served(&picture(1600, 1200, "image/jpeg"), landscape),
            "thumb"
        );
        // The same display scale for the others of the table: a square and a 16:9 picture.
        assert_eq!(
            served(&picture(2000, 2000, "image/jpeg"), asked(1.0, scale)),
            "thumb"
        );
        assert_eq!(
            served(&picture(1920, 1080, "image/jpeg"), asked(0.5625, scale)),
            "thumb"
        );
        // At scale 1 only the tall ones need the file; at 1.5 every single picture does.
        for (w, h, file_at_1) in [(4000, 3000, false), (3000, 4000, false), (1080, 2340, true)] {
            let p = picture(w, h, "image/jpeg");
            let ratio = f64::from(h) / f64::from(w);
            assert_eq!(
                served(&p, asked(ratio, 1.0)) == "file",
                file_at_1,
                "{w}x{h} at 1"
            );
            assert_eq!(served(&p, asked(ratio, 1.5)), "file", "{w}x{h} at 1.5");
        }
    }

    #[test]
    fn the_thumbnail_is_enough_exactly_when_it_keeps_what_is_asked() {
        // 4000 × 3000 keeps a tenth: its thumbnail is 400 × 300.
        let p = picture(4000, 3000, "image/jpeg");
        assert_eq!(
            served(&p, (400, 300)),
            "thumb",
            "the size of the thumbnail itself"
        );
        assert_eq!(served(&p, (400, 0)), "thumb");
        assert_eq!(served(&p, (401, 300)), "file", "one pixel more");
        assert_eq!(served(&p, (400, 301)), "file");
        // Either side alone: an unset side asks for nothing.
        assert_eq!(served(&p, (0, 300)), "thumb");
        assert_eq!(served(&p, (0, 301)), "file");
        assert_eq!(served(&p, (401, 0)), "file");
        // A side that is larger than the picture's own still means the file.
        assert_eq!(served(&p, (9999, 9999)), "file");
    }

    #[test]
    fn the_thumbnail_serves_where_nothing_would_be_gained() {
        // Nothing asked (no `sourceSize`): the thumbnail, as before.
        assert_eq!(served(&picture(3000, 4000, "image/jpeg"), (0, 0)), "thumb");
        // A picture no larger than its thumbnail: the file is no sharper.
        assert_eq!(
            served(&picture(300, 200, "image/jpeg"), (576, 384)),
            "thumb"
        );
        assert_eq!(
            served(&picture(400, 400, "image/jpeg"), (800, 800)),
            "thumb"
        );
        assert_eq!(served(&picture(401, 300, "image/jpeg"), (800, 600)), "file");
        // Unknown or empty dimensions: the thumbnail.
        let mut p = picture(3000, 4000, "image/jpeg");
        p.dims = None;
        assert_eq!(served(&p, (576, 768)), "thumb");
        p.dims = Some((0, 4000));
        assert_eq!(served(&p, (576, 768)), "thumb");
    }

    #[test]
    fn an_entry_missing_one_of_its_files_gives_the_other() {
        // Not derived yet (or a format the image crate cannot decode): the file.
        let mut p = picture(3000, 4000, "image/jpeg");
        p.thumb = None;
        assert_eq!(served(&p, (576, 768)), "file");
        assert_eq!(served(&p, (96, 96)), "file");
        assert_eq!(served(&p, (0, 0)), "file");
        // Not downloaded: the thumbnail, if one was made.
        let mut p = picture(3000, 4000, "image/jpeg");
        p.file = None;
        assert_eq!(served(&p, (576, 768)), "thumb");
        p.thumb = None;
        assert_eq!(served(&p, (576, 768)), "none");
        // The viewer's variant is unchanged: the file only.
        let p = picture(3000, 4000, "image/jpeg");
        assert_eq!(
            p.blob(MediaVariant::Full).map(|b| b.path.as_path()),
            Some(std::path::Path::new("/file"))
        );
    }

    #[test]
    fn the_pixel_limit_is_left_to_the_provider_which_knows_the_format() {
        // The format is the bytes' (`Codec::sniff` in `kourier-qt`), not the sender's word:
        // whatever the entry's MIME type says, the entry serves the file for a huge picture
        // and says that the limit applies.
        let huge = (5000, 4000);
        for mime in ["image/jpeg", "image/png", "image/webp", "IMAGE/JPEG"] {
            let p = picture(huge.0, huge.1, mime);
            assert_eq!(served(&p, (576, 432)), "file", "{mime}");
            assert!(p.exceeds_timeline_pixels(), "{mime}");
        }
        // The limit itself: 4096 × 4096 is allowed, one row more is not; nothing known, none.
        assert!(!picture(4096, 4096, "image/png").exceeds_timeline_pixels());
        assert!(picture(4096, 4097, "image/png").exceeds_timeline_pixels());
        assert!(!picture(3000, 4000, "image/png").exceeds_timeline_pixels());
        let mut unknown = picture(5000, 4000, "image/png");
        unknown.dims = None;
        assert!(!unknown.exceeds_timeline_pixels());
    }
}
