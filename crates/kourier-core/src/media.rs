//! The media handle table (§8.6): attachment id → encrypted file, key and type. The core fills
//! it whenever it answers a timeline query, so the UI's image provider resolves
//! `image://kourier/{att,thumb,blurhash}/<attachment id>` without touching `core.db` (image loads
//! never queue behind timeline queries). A miss (an evicted handle) costs one lookup on the third
//! read connection ([`MediaHandles::resolve`]). Entries are forgotten when their attachment
//! changes; keys are zeroized when an entry is dropped.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
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
    /// Its thumbnail (falls back to the full file when there is none).
    Thumb,
}

impl MediaEntry {
    /// The blob for `variant`, if there is one.
    pub fn blob(&self, variant: MediaVariant) -> Option<&MediaBlob> {
        match variant {
            MediaVariant::Full => self.file.as_ref(),
            MediaVariant::Thumb => self.thumb.as_ref().or(self.file.as_ref()),
        }
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
}
