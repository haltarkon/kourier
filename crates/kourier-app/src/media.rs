//! Media glue for the UI (§8.6, §5.8): avatar handles for the `image://kourier/avatar/…` image
//! provider and the files the composer pastes from the clipboard.
//!
//! - [`AvatarHandles`]: the core's avatar rows point at encrypted `kourier-blob` files in
//!   `accounts/<uuid>/blobs/`. The UI gets `image://kourier/avatar/<handle>?v=<rev>` (never a
//!   path or a key), and the image provider decrypts the file into memory on its own worker
//!   thread with [`AvatarHandles::read`]. No plaintext avatar is written anywhere.
//! - [`PastedFiles`]: an image pasted into the composer becomes a 0600 file in the tmpfs media
//!   cache (`$XDG_RUNTIME_DIR/kourier/media/paste/`), so `UiCommand::SendFiles` imports it like
//!   a picked file. The files go when the composer drops them, at exit and at the next start.

use std::collections::HashMap;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use kourier_blob::{FileKey, MediaCache};
use kourier_model::{AccountUuid, BlobKey, BlobRef};
use rand::RngCore;
use zeroize::Zeroizing;

/// Avatar handles kept at most; beyond that the table starts over (rows built afterwards get
/// new handles).
pub const AVATAR_HANDLES: usize = 16_384;

/// Why a media request failed. Log-safe: no paths, names or keys.
#[derive(Debug, thiserror::Error)]
pub enum MediaError {
    /// The handle is not (or no longer) registered.
    #[error("unknown media handle")]
    UnknownHandle,
    /// There is no usable plaintext media directory.
    #[error("no media cache directory")]
    NoCache,
    /// The file name is empty, too long or not a plain name.
    #[error("invalid file name")]
    InvalidName,
    /// The encrypted file is missing, truncated, tampered with, or the key is wrong.
    #[error("blob: {0}")]
    Blob(#[from] kourier_blob::BlobError),
    /// Writing a pasted file failed.
    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),
}

/// One registered avatar.
struct Avatar {
    file: PathBuf,
    key: BlobKey,
    rev: u32,
}

#[derive(Default)]
struct Table {
    by_file: HashMap<(AccountUuid, PathBuf), u64>,
    entries: HashMap<u64, Avatar>,
    next: u64,
}

/// Avatar files by handle, for the image provider (§8.6). Cheap to share; safe to use from any
/// thread.
pub struct AvatarHandles {
    accounts: PathBuf,
    table: Mutex<Table>,
}

impl std::fmt::Debug for AvatarHandles {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AvatarHandles")
            .field("len", &self.lock().entries.len())
            .finish()
    }
}

impl AvatarHandles {
    /// Avatars of the accounts below `accounts_dir` (`<data>/accounts`).
    pub fn new(accounts_dir: &Path) -> Self {
        Self {
            accounts: accounts_dir.to_owned(),
            table: Mutex::new(Table::default()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Table> {
        self.table.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The provider URL of `avatar` of `account` (`image://kourier/avatar/<handle>?v=<rev>`).
    /// Never blocks and never touches the file. The revision goes up when the same file comes
    /// back with another key, so QML's image cache does not show the old picture. `None` for a
    /// path that would leave the account's blob directory.
    pub fn url(&self, account: &AccountUuid, avatar: &BlobRef) -> Option<String> {
        // Core paths are relative to the account's blob directory and never leave it.
        if avatar.path.as_os_str().is_empty()
            || !avatar
                .path
                .components()
                .all(|c| matches!(c, Component::Normal(_)))
        {
            return None;
        }
        let mut table = self.lock();
        let id = (*account, avatar.path.clone());
        if let Some(handle) = table.by_file.get(&id).copied()
            && let Some(entry) = table.entries.get_mut(&handle)
        {
            if entry.key != avatar.key {
                entry.key = avatar.key.clone();
                entry.rev = entry.rev.wrapping_add(1);
            }
            return Some(avatar_url(handle, entry.rev));
        }
        if table.entries.len() >= AVATAR_HANDLES {
            table.by_file.clear();
            table.entries.clear();
        }
        table.next += 1;
        let handle = table.next;
        let file = self
            .accounts
            .join(account.to_string())
            .join("blobs")
            .join(&avatar.path);
        table.entries.insert(
            handle,
            Avatar {
                file,
                key: avatar.key.clone(),
                rev: 0,
            },
        );
        table.by_file.insert(id, handle);
        Some(avatar_url(handle, 0))
    }

    /// Decrypts the avatar `handle` into memory (zeroized on drop), verifying every chunk.
    /// Blocking: the image provider's worker thread calls it, never the Qt thread.
    pub fn read(&self, handle: u64) -> Result<Zeroizing<Vec<u8>>, MediaError> {
        let (file, key) = {
            let table = self.lock();
            let entry = table
                .entries
                .get(&handle)
                .ok_or(MediaError::UnknownHandle)?;
            (entry.file.clone(), FileKey::from_bytes(*entry.key.expose()))
        };
        Ok(kourier_blob::read_all(&file, &key)?)
    }

    /// Number of registered avatars.
    pub fn len(&self) -> usize {
        self.lock().entries.len()
    }

    /// Whether no avatar is registered.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

fn avatar_url(handle: u64, rev: u32) -> String {
    format!("image://kourier/avatar/{handle}?v={rev}")
}

/// Files pasted into the composer, in the tmpfs media cache.
pub struct PastedFiles {
    cache: Option<MediaCache>,
    /// Directories made by [`PastedFiles::save`] (one per file, so the file keeps its name).
    dirs: Mutex<Vec<PathBuf>>,
}

impl std::fmt::Debug for PastedFiles {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PastedFiles")
            .field("enabled", &self.cache.is_some())
            .field(
                "files",
                &self
                    .dirs
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .len(),
            )
            .finish()
    }
}

impl PastedFiles {
    /// Pasted files below `media_dir/paste` (wiped now: leftovers of a previous run go). Without
    /// a usable directory pasting is refused.
    pub fn new(media_dir: &Path) -> Self {
        let cache = match MediaCache::open(&media_dir.join("paste")) {
            Ok(cache) => {
                if !cache.is_tmpfs() {
                    tracing::warn!("the paste directory is not on tmpfs (KOURIER_HOME on a disk?)");
                }
                Some(cache)
            }
            Err(e) => {
                tracing::warn!(error = %e, "no paste directory; pasting images is off");
                None
            }
        };
        Self {
            cache,
            dirs: Mutex::new(Vec::new()),
        }
    }

    /// Writes `bytes` as a new 0600 file called `name` (a plain file name such as
    /// `Pasted image.png`) and returns its path for `SendFiles`. Blocking (small files).
    pub fn save(&self, name: &str, bytes: &[u8]) -> Result<PathBuf, MediaError> {
        let cache = self.cache.as_ref().ok_or(MediaError::NoCache)?;
        if name.is_empty()
            || name.len() > 200
            || name.contains(['/', '\0'])
            || name == "."
            || name == ".."
        {
            return Err(MediaError::InvalidName);
        }
        let mut random = [0u8; 12];
        rand::rng().fill_bytes(&mut random);
        let dir = cache.dir().join(
            random
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>(),
        );
        std::fs::DirBuilder::new().mode(0o700).create(&dir)?;
        self.dirs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(dir.clone());
        let path = dir.join(name);
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        file.write_all(bytes)?;
        file.flush()?;
        Ok(path)
    }

    /// Whether `path` is a file [`save`](Self::save) made.
    pub fn owns(&self, path: &Path) -> bool {
        let Some(parent) = path.parent() else {
            return false;
        };
        self.dirs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .any(|d| d == parent)
    }

    /// Deletes a file [`save`](Self::save) made; other paths are left alone. Returns whether it
    /// was one of ours.
    pub fn release(&self, path: &Path) -> bool {
        let Some(parent) = path.parent() else {
            return false;
        };
        let mut dirs = self.dirs.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(at) = dirs.iter().position(|d| d == parent) else {
            return false;
        };
        let dir = dirs.swap_remove(at);
        if let Err(e) = std::fs::remove_dir_all(&dir) {
            tracing::debug!(error = %e, "removing a pasted file failed");
        }
        true
    }

    /// Deletes every pasted file (exit).
    pub fn clear(&self) {
        self.dirs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
        // A directory that is gone already (the runtime directory went first) is clean.
        if let Some(cache) = &self.cache
            && let Err(e) = cache.clear()
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(error = %e, "wiping the paste directory failed");
        }
    }
}

impl Drop for PastedFiles {
    fn drop(&mut self) {
        self.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn avatar_file(dir: &Path, account: &AccountUuid, rel: &str, data: &[u8]) -> BlobRef {
        let path = dir
            .join("accounts")
            .join(account.to_string())
            .join("blobs")
            .join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let key = FileKey::generate();
        kourier_blob::write_all(&path, &key, data).unwrap();
        BlobRef {
            path: PathBuf::from(rel),
            key: BlobKey::new(*key.as_bytes()),
        }
    }

    #[test]
    fn avatar_handles_decrypt_in_memory_and_bump_on_a_new_key() {
        let dir = tempfile::tempdir().unwrap();
        let account = AccountUuid::new_random();
        let avatar = avatar_file(dir.path(), &account, "ab/avatar", b"png");
        let handles = AvatarHandles::new(&dir.path().join("accounts"));
        let url = handles.url(&account, &avatar).unwrap();
        assert_eq!(url, "image://kourier/avatar/1?v=0");
        assert_eq!(
            handles.url(&account, &avatar).as_deref(),
            Some(url.as_str())
        );
        assert_eq!(handles.read(1).unwrap().as_slice(), b"png");
        assert!(matches!(handles.read(7), Err(MediaError::UnknownHandle)));

        // The same file rewritten with another key: same handle, next revision.
        let again = avatar_file(dir.path(), &account, "ab/avatar", b"new");
        assert_eq!(
            handles.url(&account, &again).as_deref(),
            Some("image://kourier/avatar/1?v=1")
        );
        assert_eq!(handles.read(1).unwrap().as_slice(), b"new");
        assert_eq!(handles.len(), 1);
    }

    #[test]
    fn a_wrong_key_or_an_escaping_path_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let account = AccountUuid::new_random();
        let mut avatar = avatar_file(dir.path(), &account, "cd/avatar", b"png");
        avatar.key = BlobKey::new([9; 32]);
        let handles = AvatarHandles::new(&dir.path().join("accounts"));
        let url = handles.url(&account, &avatar).unwrap();
        let handle: u64 = url
            .strip_prefix("image://kourier/avatar/")
            .and_then(|s| s.split('?').next())
            .and_then(|s| s.parse().ok())
            .unwrap();
        assert!(matches!(handles.read(handle), Err(MediaError::Blob(_))));
        for bad in ["../../../etc/passwd", "/etc/passwd", ""] {
            let escape = BlobRef {
                path: PathBuf::from(bad),
                key: BlobKey::new([0; 32]),
            };
            assert_eq!(handles.url(&account, &escape), None, "{bad}");
        }
        assert!(!format!("{handles:?}").contains("avatar"));
    }

    #[test]
    fn pasted_files_are_private_and_go_on_release_and_clear() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let pasted = PastedFiles::new(&dir.path().join("media"));
        let a = pasted.save("Pasted image.png", b"\x89PNG").unwrap();
        assert_eq!(a.file_name().unwrap(), "Pasted image.png");
        assert!(a.starts_with(dir.path().join("media/paste")));
        assert_eq!(
            std::fs::metadata(&a).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(std::fs::read(&a).unwrap(), b"\x89PNG");
        assert!(pasted.owns(&a));
        let b = pasted.save("notes.txt", b"hi").unwrap();
        assert!(!pasted.release(Path::new("/etc/passwd")));
        assert!(pasted.release(&a));
        assert!(!a.exists() && !a.parent().unwrap().exists());
        assert!(!pasted.owns(&a));
        for bad in ["", "a/b", "..", "x\0y"] {
            assert!(matches!(
                pasted.save(bad, b""),
                Err(MediaError::InvalidName)
            ));
        }
        pasted.clear();
        assert!(!b.exists());
        assert_eq!(
            std::fs::read_dir(dir.path().join("media/paste"))
                .unwrap()
                .count(),
            0
        );
    }
}
