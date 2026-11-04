//! The files of an upload (W5b, decision G3; `.claude/rules/upload-progress.md`, sealed temporary files): the core's
//! file the attempt reads, and the ONE temporary file it writes.
//!
//! THE CORE'S FILE: a `Body::Media` attachment names its file by a path RELATIVE to `AccountContext::blob_dir` (the
//! core's encrypted blob store); [`core_file`] joins it only when it stays inside (Signal's rule, `is_relative_inside`
//! in `kourier-signal/src/send/media.rs`): an absolute path or a `..` is `InvalidInput`, never opened.
//!
//! THE TEMPORARY FILE ([`TempFile`]): WhatsApp's ciphertext of the file (AES-CBC + MAC under the message's media key)
//! is written ONCE, before the upload, so its length and hashes are final before the first byte goes up and a second
//! host or a resume reads the same bytes. It is SEALED once more as a `kourier-blob` under a fresh [`FileKey`] held in
//! memory only, so the rule stays one sentence: every temporary upload file is a kourier-blob under a key in memory
//! (no reviewer has to argue about a ciphertext whose media key may sit in `backend.db` once the upload is recorded).
//! It lives in ONE directory per account, `upload-tmp` beside `backend.db` (not in the blob store: the core's orphan
//! sweep never looks there), 0700, the file 0600. Plaintext is never on disk.
//!
//! THE END: the file goes when the LAST holder of its [`TempFile`] lets go (the attempt and the upload's blocking
//! reader share it through an `Arc`): sent, failed, stopped or dropped with its task, and a blocking read that ends
//! after its attempt removes it then. [`clean_at_start`] empties the directory when the account's actor starts, for a
//! process that was killed in the middle of an upload (the key died with it). The directory itself stays while the
//! actor runs (attempts of one account run side by side and share it).

use std::path::{Component, Path, PathBuf};

use kourier_blob::{BlobError, BlobId, BlobReader, BlobStore, BlobWriter, FileKey};
use kourier_model::{BackendError, ErrorKind};

use super::upload::STALL_RETRY;

/// The directory's name beside `backend.db`.
pub(crate) const DIR: &str = "upload-tmp";

/// The directory of an account's temporary upload files: beside its `backend.db`.
pub(crate) fn dir_beside(backend_db: &Path) -> PathBuf {
    backend_db
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(DIR)
}

/// Removes the directory with whatever it holds, when the account's actor starts (blocking; a missing one is fine):
/// an upload that was running when the process died left its file there, and its key is gone with the process.
pub(crate) fn clean_at_start(dir: &Path) {
    match std::fs::remove_dir_all(dir) {
        Ok(()) => tracing::debug!("the temporary upload files of a dead process were removed"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            tracing::warn!(error = %e.kind(), "removing the temporary upload files failed");
        }
    }
}

/// Where the core's file of an attachment is: `relative` joined to `blob_dir`, only when it stays inside it.
pub(crate) fn core_file(blob_dir: &Path, relative: &Path) -> Result<PathBuf, BackendError> {
    let inside = !relative.as_os_str().is_empty()
        && relative
            .components()
            .all(|c| matches!(c, Component::Normal(_) | Component::CurDir));
    if !inside {
        return Err(BackendError::new(
            ErrorKind::InvalidInput,
            "an attachment file outside the blob directory",
        ));
    }
    Ok(blob_dir.join(relative))
}

/// The one temporary file of an attempt: a kourier-blob under a key that exists in memory only. Removed when it is
/// dropped (share it through an `Arc` with the upload's reader: the last holder removes it).
pub(crate) struct TempFile {
    path: PathBuf,
    key: FileKey,
}

impl TempFile {
    /// A new file in `dir` (the directory is made 0700 when missing) and the writer that fills it. The writer writes
    /// beside the file and puts it in place at [`Self::finish`]; an unfinished writer removes its own part when it
    /// is dropped. The task that writes and finishes holds the guard (an `Arc` clone) until it is done, so a file
    /// finished after its attempt ended is removed by that task. Blocking.
    pub(crate) fn create(dir: &Path) -> Result<(Self, BlobWriter), BackendError> {
        BlobStore::open(dir).map_err(|e| failed(&BlobError::from_io(e)))?;
        let file = Self {
            path: dir.join(BlobId::new_random().as_str()),
            key: FileKey::generate(),
        };
        let writer = BlobWriter::create(&file.path, &file.key).map_err(|e| failed(&e))?;
        Ok((file, writer))
    }

    /// Ends the writer's file (no fsync: a kill loses nothing that is not made again from the core's file) and gives
    /// the length it holds. Blocking.
    pub(crate) fn finish(writer: BlobWriter) -> Result<u64, BackendError> {
        writer.finish_volatile().map_err(|e| failed(&e))
    }

    /// A reader of what was written, from the start. Blocking.
    pub(crate) fn reader(&self) -> std::io::Result<BlobReader<std::fs::File>> {
        BlobReader::open(&self.path, &self.key).map_err(std::io::Error::other)
    }

    /// Where the file is (tests).
    #[cfg(test)]
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

/// A failed write of the temporary file: the disk is full or the directory cannot be written; the next attempt may
/// succeed. The detail names the kind only.
fn failed(e: &BlobError) -> BackendError {
    tracing::warn!(error = %e, "the temporary upload file could not be written");
    unwritable()
}

/// The answer to a temporary file that could not be written (here, and in `upload`'s encryption): `Transient` with the
/// stall floor, never at once. Every try reads and encrypts the whole file (up to 2 GB) before a full disk fails it
/// again, and free space does not come back in a moment (re-check N3; the floor of fix F18's `busy`).
pub(crate) fn unwritable() -> BackendError {
    BackendError::new(
        ErrorKind::Transient {
            retry_after: Some(STALL_RETRY),
        },
        "the file to send could not be prepared",
    )
}

impl Drop for TempFile {
    fn drop(&mut self) {
        match std::fs::remove_file(&self.path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                tracing::warn!(error = %e.kind(), "removing a temporary upload file failed");
            }
        }
    }
}

#[cfg(test)]
mod tests;
