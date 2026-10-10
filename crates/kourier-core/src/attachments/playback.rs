//! Plaintext media files for playback (§5.8, §8.6): GIFs, animated stickers, video and voice
//! notes are decrypted into the tmpfs media cache (`$XDG_RUNTIME_DIR/kourier/media`) while a view
//! shows them, because `AnimatedImage` and Qt Multimedia cannot read through an image provider.
//! Opens are counted per attachment; the file is deleted when the last view closes it, at exit,
//! and at startup (the cache is wiped when it opens).
//!
//! The core owns only the [`PLAYBACK_DIR`] subdirectory of the media cache it is given: the app
//! keeps other plaintext caches beside it (decrypted avatars in `avatars/`), and wiping the
//! whole cache at start or exit would delete files the app still shows.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use kourier_blob::{FileKey, MediaCache, MediaFile};
use kourier_model::{AttachmentId, BlobKey};

use crate::error::{CoreError, Result};

/// The core's subdirectory of the media cache (`<media cache>/attachments`).
pub(crate) const PLAYBACK_DIR: &str = "attachments";

/// The open plaintext files.
pub(crate) struct Playback {
    cache: Option<MediaCache>,
    open: Mutex<HashMap<AttachmentId, (Arc<MediaFile>, u32)>>,
}

impl std::fmt::Debug for Playback {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Playback")
            .field("enabled", &self.cache.is_some())
            .finish_non_exhaustive()
    }
}

impl Playback {
    /// Opens (and wipes) the core's [`PLAYBACK_DIR`] inside the media cache `dir`; without one,
    /// playback files are refused. Anything else in `dir` is left alone.
    pub(crate) fn new(dir: Option<&Path>) -> Self {
        let cache = dir.and_then(|d| match MediaCache::open(&d.join(PLAYBACK_DIR)) {
            Ok(c) => {
                if !c.is_tmpfs() {
                    tracing::warn!(
                        "the media cache is not on tmpfs: plaintext media files reach a disk"
                    );
                }
                Some(c)
            }
            Err(e) => {
                tracing::warn!(error = %e, "opening the media cache failed; no media playback");
                None
            }
        });
        Self {
            cache,
            open: Mutex::new(HashMap::new()),
        }
    }

    fn map(&self) -> std::sync::MutexGuard<'_, HashMap<AttachmentId, (Arc<MediaFile>, u32)>> {
        self.open.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The plaintext file of `attachment`, decrypted from `blob` on the first open (blocking).
    /// Every open needs one [`close`](Self::close).
    pub(crate) fn open(
        &self,
        attachment: AttachmentId,
        blob: &Path,
        key: &BlobKey,
        mime: Option<&str>,
        file_name: Option<&str>,
    ) -> Result<PathBuf> {
        if let Some((file, n)) = self.map().get_mut(&attachment) {
            *n += 1;
            return Ok(file.path().to_owned());
        }
        let cache = self
            .cache
            .as_ref()
            .ok_or(CoreError::InvalidState("no media cache directory"))?;
        let ext = extension(mime, file_name);
        let file = cache
            .materialize(blob, &FileKey::from_bytes(*key.expose()), ext)
            .map_err(CoreError::Blob)?;
        let mut map = self.map();
        // Another open may have finished first: keep its file, drop ours.
        let (file, n) = map.entry(attachment).or_insert_with(|| (Arc::new(file), 0));
        *n += 1;
        Ok(file.path().to_owned())
    }

    /// One view closed `attachment`; the file goes with the last one. Returns whether it went.
    pub(crate) fn close(&self, attachment: AttachmentId) -> bool {
        let mut map = self.map();
        let Some((_, n)) = map.get_mut(&attachment) else {
            return false;
        };
        *n = n.saturating_sub(1);
        if *n == 0 {
            map.remove(&attachment);
            return true;
        }
        false
    }

    /// Drops the files of attachments that went (their message was deleted).
    pub(crate) fn forget(&self, ids: impl IntoIterator<Item = AttachmentId>) {
        let mut map = self.map();
        for id in ids {
            map.remove(&id);
        }
    }

    /// Deletes every plaintext file (exit).
    pub(crate) fn clear(&self) {
        self.map().clear();
        if let Some(c) = &self.cache
            && let Err(e) = c.clear()
        {
            tracing::warn!(error = %e, "wiping the media cache failed");
        }
    }
}

/// A file-name extension Qt can tell the type from: from the MIME type, else from the original
/// name (ASCII alphanumerics, ≤ 16), else none.
fn extension<'a>(mime: Option<&str>, file_name: Option<&'a str>) -> &'a str {
    if let Some(e) = mime.and_then(kourier_media::extension_for_mime) {
        return e;
    }
    file_name
        .and_then(|n| Path::new(n).extension())
        .and_then(|e| e.to_str())
        .filter(|e| e.len() <= 16 && e.bytes().all(|b| b.is_ascii_alphanumeric()))
        .unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_are_counted_and_the_last_close_deletes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let blob = dir.path().join("blob");
        let key = FileKey::generate();
        kourier_blob::write_all(&blob, &key, b"GIF89a frames").expect("blob");
        let media = dir.path().join("media");
        // The app's own cache beside the core's: never touched.
        let avatars = media.join("avatars");
        std::fs::create_dir_all(&avatars).expect("avatars");
        std::fs::write(avatars.join("a.png"), b"avatar").expect("avatar");
        let cache = media.join(PLAYBACK_DIR);
        std::fs::create_dir_all(&cache).expect("cache");
        std::fs::write(cache.join("left-behind.gif"), b"old run").expect("leftover");
        let p = Playback::new(Some(&media));
        assert_eq!(
            std::fs::read_dir(&cache).expect("dir").count(),
            0,
            "a previous run's files go at start"
        );
        let bkey = BlobKey::new(*key.as_bytes());
        let id = AttachmentId(3);
        let a = p
            .open(id, &blob, &bkey, Some("image/gif"), None)
            .expect("open");
        let b = p
            .open(id, &blob, &bkey, Some("image/gif"), None)
            .expect("open");
        assert_eq!(a, b);
        assert_eq!(a.extension().and_then(|e| e.to_str()), Some("gif"));
        assert_eq!(std::fs::read(&a).expect("plaintext"), b"GIF89a frames");
        assert!(!p.close(id));
        assert!(a.exists());
        assert!(p.close(id));
        assert!(!a.exists(), "the last close deletes the plaintext");
        assert!(!p.close(id));

        let c = p
            .open(AttachmentId(4), &blob, &bkey, None, Some("clip.ogg"))
            .expect("open");
        assert_eq!(c.extension().and_then(|e| e.to_str()), Some("ogg"));
        p.clear();
        assert!(!c.exists(), "exit wipes the cache");
        assert!(
            Playback::new(None)
                .open(id, &blob, &bkey, None, None)
                .is_err()
        );
        // A wrong key leaves nothing behind.
        assert!(
            p.open(AttachmentId(5), &blob, &BlobKey::new([0; 32]), None, None)
                .is_err()
        );
        assert_eq!(std::fs::read_dir(&cache).expect("dir").count(), 0);
        assert_eq!(
            std::fs::read(avatars.join("a.png")).expect("the app's cache stays"),
            b"avatar"
        );
        assert_eq!(extension(None, Some("x.tar.gz")), "gz");
        assert_eq!(extension(None, Some("weird.ext!")), "");
    }
}
