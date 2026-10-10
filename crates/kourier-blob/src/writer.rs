//! Atomic, streaming blob writer.

use crate::format::{Sealer, TAG_LEN};
use crate::{BlobError, FileKey};
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use zeroize::Zeroizing;

/// Encrypts a stream of plaintext into `<path>.tmp` and renames it to `path` on
/// [`finish`](Self::finish). Dropping it unfinished removes the temporary file, so `path` either
/// holds a complete blob or does not exist.
pub struct BlobWriter {
    file: Option<File>,
    tmp: PathBuf,
    path: PathBuf,
    sealer: Sealer,
    buf: Zeroizing<Vec<u8>>,
    index: u32,
    len: u64,
}

impl BlobWriter {
    /// Starts a new blob at `path` (its directory must exist). The file is created 0600.
    pub fn create(path: &Path, key: &FileKey) -> Result<Self, BlobError> {
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);
        let open = || {
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&tmp)
        };
        let mut file = match open() {
            // A leftover from a crashed write of the same id: start over.
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                std::fs::remove_file(&tmp)?;
                open()?
            }
            other => other?,
        };
        let sealer = Sealer::new(key);
        if let Err(e) = file.write_all(&sealer.header) {
            drop(file);
            let _ = std::fs::remove_file(&tmp);
            return Err(e.into());
        }
        let cap = sealer.chunk_size + TAG_LEN;
        Ok(Self {
            file: Some(file),
            tmp,
            path: path.to_owned(),
            sealer,
            buf: Zeroizing::new(Vec::with_capacity(cap)),
            index: 0,
            len: 0,
        })
    }

    /// Seals the last chunk, fsyncs, renames into place and fsyncs the directory. Returns the
    /// plaintext length.
    pub fn finish(mut self) -> Result<u64, BlobError> {
        self.seal_buffered(true)?;
        // Until the rename succeeded, an error leaves the file in `self` and Drop removes the tmp.
        self.file.as_ref().ok_or(BlobError::Corrupt)?.sync_all()?;
        std::fs::rename(&self.tmp, &self.path)?;
        drop(self.file.take());
        if let Some(dir) = self.path.parent() {
            File::open(dir)?.sync_all()?;
        }
        Ok(self.len)
    }

    /// [`finish`](Self::finish) without the two fsyncs: seals the last chunk and renames
    /// into place. For a file that can be made again from data that is durable (a
    /// thumbnail of a stored picture): after a power loss it may be missing or cut short,
    /// which its owner finds by its length ([`sealed_len`](crate::sealed_len)) and a reader
    /// by the missing last chunk. Returns the plaintext length.
    pub fn finish_volatile(mut self) -> Result<u64, BlobError> {
        self.seal_buffered(true)?;
        std::fs::rename(&self.tmp, &self.path)?;
        drop(self.file.take());
        Ok(self.len)
    }

    fn seal_buffered(&mut self, last: bool) -> Result<(), BlobError> {
        if !last && self.index == u32::MAX {
            return Err(BlobError::TooLarge);
        }
        self.sealer.seal(self.index, last, &mut self.buf)?;
        self.file
            .as_mut()
            .ok_or(BlobError::Corrupt)?
            .write_all(&self.buf)?;
        self.buf.clear();
        self.index = self.index.wrapping_add(1);
        Ok(())
    }
}

impl Write for BlobWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if data.is_empty() {
            return Ok(0);
        }
        // A full chunk is sealed as "not last" only once more data arrives: the last chunk may be
        // full-size, and it must carry the last flag.
        if self.buf.len() == self.sealer.chunk_size {
            self.seal_buffered(false).map_err(BlobError::into_io)?;
        }
        let n = data.len().min(self.sealer.chunk_size - self.buf.len());
        self.buf.extend_from_slice(&data[..n]);
        self.len += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for BlobWriter {
    fn drop(&mut self) {
        if self.file.take().is_some() {
            let _ = std::fs::remove_file(&self.tmp);
        }
    }
}
