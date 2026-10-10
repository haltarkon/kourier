//! The plaintext media cache: `$XDG_RUNTIME_DIR/kourier/media/` (tmpfs, 0700), §5.8.
//!
//! Plaintext files exist only for AnimatedImage, video and audio playback and for "save/open
//! file". Each is a [`MediaFile`] that deletes itself when dropped (the view closed); the whole
//! directory is wiped when the cache is opened (startup) and by [`MediaCache::clear`] (exit).

use crate::store::create_private_dir;
use crate::{BlobError, BlobReader, FileKey};
use rand::RngCore;
use std::ffi::CString;
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

const TMPFS_MAGIC: i64 = 0x0102_1994;
const RAMFS_MAGIC: i64 = 0x8584_58f6;

/// The plaintext media directory.
#[derive(Debug)]
pub struct MediaCache {
    dir: PathBuf,
    tmpfs: bool,
}

/// A decrypted file in the cache; removed on drop.
#[derive(Debug)]
pub struct MediaFile {
    path: PathBuf,
}

impl MediaFile {
    /// The plaintext file (hand it to Qt as a `file://` URL).
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for MediaFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

impl MediaCache {
    /// Opens the cache at `dir` (for example `$XDG_RUNTIME_DIR/kourier/media`), creating it 0700
    /// and deleting whatever a previous run left behind. Whether `dir` is on tmpfs is reported by
    /// [`is_tmpfs`](Self::is_tmpfs); with `KOURIER_HOME` on a disk it is not, and the caller warns.
    pub fn open(dir: &Path) -> io::Result<Self> {
        create_private_dir(dir)?;
        let cache = Self {
            dir: dir.to_owned(),
            tmpfs: is_memory_fs(dir)?,
        };
        cache.clear()?;
        Ok(cache)
    }

    /// The cache directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Whether the directory is on tmpfs or ramfs (plaintext never reaches a disk).
    pub fn is_tmpfs(&self) -> bool {
        self.tmpfs
    }

    /// Decrypts blob `blob` into a new 0600 file named `<random>.<ext>` (`ext`: up to 16 ASCII
    /// alphanumerics, so Qt can tell the type; empty for none). The file appears to the caller only
    /// once every chunk authenticated; on any error it is removed.
    pub fn materialize(
        &self,
        blob: &Path,
        key: &FileKey,
        ext: &str,
    ) -> Result<MediaFile, BlobError> {
        if ext.len() > 16 || !ext.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return Err(BlobError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid media file extension",
            )));
        }
        let mut reader = BlobReader::open(blob, key)?;
        let mut name = [0u8; 16];
        rand::rng().fill_bytes(&mut name);
        let mut file_name: String = name.iter().map(|b| format!("{b:02x}")).collect();
        if !ext.is_empty() {
            file_name.push('.');
            file_name.push_str(ext);
        }
        // Owning the path first means an error below removes the partial plaintext.
        let media = MediaFile {
            path: self.dir.join(file_name),
        };
        let mut out = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&media.path)?;
        io::copy(&mut reader, &mut out).map_err(BlobError::from_io)?;
        out.flush()?;
        Ok(media)
    }

    /// Deletes every file in the cache (startup and exit).
    pub fn clear(&self) -> io::Result<()> {
        for entry in std::fs::read_dir(&self.dir)? {
            let entry = entry?;
            let path = entry.path();
            if entry.file_type()?.is_dir() {
                std::fs::remove_dir_all(&path)?;
            } else {
                std::fs::remove_file(&path)?;
            }
        }
        Ok(())
    }
}

/// `statfs(2)`: is `path` on tmpfs or ramfs?
fn is_memory_fs(path: &Path) -> io::Result<bool> {
    let c_path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
    // SAFETY: `statfs` is plain old data, so all-zero is a valid value.
    let mut buf: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c_path` is a valid NUL-terminated string and `buf` a valid out pointer for the
    // duration of the call.
    let rc = unsafe { libc::statfs(c_path.as_ptr(), &mut buf) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    #[allow(clippy::unnecessary_cast)] // `f_type` is i64 on x86_64 but differs on other targets
    let fs_type = buf.f_type as i64;
    Ok(fs_type == TMPFS_MAGIC || fs_type == RAMFS_MAGIC)
}
