//! The temporary files of an attempt's re-encoded pictures (upload progress, R-Q1).
//!
//! Every still picture of a message is re-encoded BEFORE the first byte goes up, so the size of
//! each upload is final and the share never jumps down. Holding them all would cost up to
//! ~93 MiB for an album of 31, so each goes into a file of its own, encrypted under a key made
//! for it ([`kourier_blob`]; the key lives in memory only) in ONE directory per account,
//! `accounts/<uuid>/upload-tmp`, beside `backend.db` and not in the blob store (the core's
//! orphan sweep never looks there). Plaintext is never on disk.
//!
//! The files end with their attempt: [`TempFiles`] removes what it made when it is dropped,
//! whether the attempt was sent, failed, aborted or dropped with its task, and
//! [`clean_at_start`] empties the directory when the account's actor starts, for a process that
//! was killed in the middle of a send. The directory itself stays (several attempts of one
//! account run side by side and share it).

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use kourier_blob::{BlobId, BlobStore, BlobWriter, FileKey};
use kourier_model::{BackendError, BlobKey};

/// The directory's name beside `backend.db`.
pub(crate) const DIR: &str = "upload-tmp";

/// The directory of an account's temporary upload files: beside its `backend.db`.
pub(crate) fn dir_beside(backend_db: &Path) -> PathBuf {
    backend_db
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(DIR)
}

/// Removes the directory with whatever it holds, when the account's actor starts (blocking; a
/// missing one is fine): a send that was running when the process died left files in it, and
/// their keys are gone with the process.
pub(crate) fn clean_at_start(dir: &Path) {
    match std::fs::remove_dir_all(dir) {
        Ok(()) => tracing::debug!("the temporary upload files of a dead process were removed"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            tracing::warn!(error = %e.kind(), "removing the temporary upload files failed");
        }
    }
}

/// The temporary files of one attempt. Share it (`Arc`) with every blocking task that writes
/// one: the files go when the LAST holder lets go, so a write that finishes after its attempt
/// was dropped is removed too.
pub(crate) struct TempFiles {
    dir: PathBuf,
    made: Mutex<Vec<PathBuf>>,
}

impl TempFiles {
    /// An attempt's files in `dir` (made on the first write, 0700).
    pub(crate) fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            made: Mutex::new(Vec::new()),
        }
    }

    /// Seals `bytes` into a new file under a fresh key. Returns where it is, its key and its
    /// plaintext length. Blocking.
    pub(crate) fn write(&self, bytes: &[u8]) -> Result<(PathBuf, BlobKey, u64), BackendError> {
        let failed = |e: &dyn std::fmt::Display| {
            tracing::warn!(error = %e, "a temporary upload file could not be written");
            BackendError::transient("the file to send could not be prepared")
        };
        BlobStore::open(&self.dir).map_err(|e| failed(&e))?;
        let key = FileKey::generate();
        let path = self.dir.join(BlobId::new_random().as_str());
        // Made again from the original in a moment: no fsync (`finish_volatile`).
        let mut writer = BlobWriter::create(&path, &key).map_err(|e| failed(&e))?;
        writer.write_all(bytes).map_err(|e| failed(&e))?;
        let len = writer.finish_volatile().map_err(|e| failed(&e))?;
        self.made
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(path.clone());
        Ok((path, BlobKey::new(*key.as_bytes()), len))
    }

    /// Where the files of this attempt are (tests).
    #[cfg(test)]
    pub(crate) fn made(&self) -> Vec<PathBuf> {
        self.made
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl Drop for TempFiles {
    fn drop(&mut self) {
        let made = self.made.get_mut().unwrap_or_else(PoisonError::into_inner);
        for path in made.drain(..) {
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    tracing::warn!(error = %e.kind(), "removing a temporary upload file failed");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    fn scratch() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    /// A file is a `kourier-blob` sealed under its own key (0600 in a 0700 directory), reads
    /// back with that key only, and goes with its attempt.
    #[test]
    fn a_temporary_file_is_sealed_private_and_goes_with_its_attempt() {
        let root = scratch();
        let dir = dir_beside(&root.path().join("backend.db"));
        assert_eq!(dir.file_name().and_then(|n| n.to_str()), Some("upload-tmp"));
        let files = TempFiles::new(dir.clone());
        let plain = b"a picture that must not touch the disk as it is".repeat(500);
        let (path, key, len) = files.write(&plain).expect("write");
        assert_eq!(len, plain.len() as u64);
        let sealed = std::fs::read(&path).expect("read");
        assert_eq!(
            &sealed[..8],
            &kourier_blob::MAGIC[..],
            "a kourier-blob file"
        );
        assert_eq!(sealed.len() as u64, kourier_blob::sealed_len(len));
        assert!(
            !sealed
                .windows(plain.len().min(40))
                .any(|w| w == &plain[..40]),
            "the plaintext is not in the file"
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let read = kourier_blob::read_all(&path, &FileKey::from_bytes(*key.expose())).expect("key");
        assert_eq!(&read[..], &plain[..]);
        assert!(
            kourier_blob::read_all(&path, &FileKey::generate()).is_err(),
            "no other key opens it"
        );
        // Two files of one attempt have two keys.
        let (second, other_key, _) = files.write(b"x").expect("write");
        assert_ne!(key.expose(), other_key.expose());
        assert_eq!(files.made(), [path.clone(), second.clone()]);
        drop(files);
        assert!(!path.exists() && !second.exists(), "gone with the attempt");
        assert!(dir.is_dir(), "the directory stays for the next attempt");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
    }

    /// A process that died in the middle of a send ran no destructor: its files are removed
    /// when the next actor of the account starts, and a missing directory is fine.
    #[test]
    fn a_start_removes_what_a_dead_process_left() {
        let root = scratch();
        let dir = dir_beside(&root.path().join("backend.db"));
        clean_at_start(&dir);
        let files = TempFiles::new(dir.clone());
        let (a, ..) = files.write(&[1; 100]).expect("write");
        let (b, ..) = files.write(&[2; 100]).expect("write");
        // The kill: nothing runs the guard.
        std::mem::forget(files);
        assert!(a.exists() && b.exists());
        clean_at_start(&dir);
        assert!(!dir.exists(), "the directory and its files are gone");
        clean_at_start(&dir);
    }
}
