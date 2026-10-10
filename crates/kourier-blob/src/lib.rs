//! Encrypted attachment files and the tmpfs plaintext media cache (docs/architecture.md §5.8).
//!
//! Format `kourier-blob v1`: a 20-byte header (magic `KOURBLOB`, version 1, chunk size as u32
//! LE = 64 KiB, 7-byte random nonce prefix), then AES-256-GCM STREAM chunks (`StreamBE32`: a
//! 32-bit big-endian chunk counter and a last-chunk flag complete the 12-byte nonce). Every chunk
//! authenticates the header as associated data. Keys are random per file and live in `core.db`.
//! A reader detects tampering, a wrong key and truncation (a missing last chunk); it streams, so
//! a consumer may trust the bytes only once it read to the end without an error.
//!
//! Writes are atomic: `<id>.tmp`, fsync, rename, fsync of the directory; files are 0600 and
//! directories 0700. Plaintext exists only in the [`MediaCache`] (tmpfs) and in memory.

mod format;
mod media;
mod reader;
mod store;
mod writer;

pub use format::{CHUNK_SIZE, HEADER_LEN, MAGIC, VERSION};
pub use media::{MediaCache, MediaFile};
pub use reader::BlobReader;
pub use store::{BlobId, BlobStore};
pub use writer::BlobWriter;

use rand::RngCore;
use std::fmt;
use std::io;
use zeroize::Zeroizing;

/// Errors of this crate.
#[derive(Debug, thiserror::Error)]
pub enum BlobError {
    /// File system error.
    #[error(transparent)]
    Io(#[from] io::Error),
    /// The file does not start with the `kourier-blob` magic.
    #[error("not a kourier-blob file")]
    NotABlob,
    /// A format version this build does not read.
    #[error("unsupported kourier-blob version {0}")]
    UnsupportedVersion(u8),
    /// The header names an impossible chunk size.
    #[error("invalid kourier-blob header")]
    BadHeader,
    /// The file ends before its last chunk.
    #[error("kourier-blob file is truncated")]
    Truncated,
    /// A chunk failed authentication: tampered, cut mid-chunk, or the wrong key.
    #[error("kourier-blob file is corrupt or the key is wrong")]
    Corrupt,
    /// The file is larger than STREAM's 2^32 chunks.
    #[error("kourier-blob file is too large")]
    TooLarge,
}

impl BlobError {
    /// Recovers a `BlobError` that a [`BlobReader`] or [`BlobWriter`] returned through
    /// `std::io` (as `ErrorKind::InvalidData` / `Other`).
    pub fn from_io(e: io::Error) -> Self {
        match e
            .get_ref()
            .and_then(|inner| inner.downcast_ref::<BlobError>())
        {
            Some(_) => match e.into_inner().map(|inner| inner.downcast::<BlobError>()) {
                Some(Ok(blob)) => *blob,
                _ => BlobError::Corrupt,
            },
            None => BlobError::Io(e),
        }
    }

    pub(crate) fn into_io(self) -> io::Error {
        match self {
            BlobError::Io(e) => e,
            other => io::Error::new(io::ErrorKind::InvalidData, other),
        }
    }
}

/// A random per-file AES-256 key, zeroized on drop. The core stores it in `files.key`.
#[derive(Clone)]
pub struct FileKey(Zeroizing<[u8; 32]>);

impl FileKey {
    /// A fresh random key.
    pub fn generate() -> Self {
        let mut k = Zeroizing::new([0u8; 32]);
        rand::rng().fill_bytes(k.as_mut());
        Self(k)
    }

    /// Wraps stored key bytes.
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(Zeroizing::new(bytes))
    }

    /// The raw key bytes (for storing in `core.db`).
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Debug for FileKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("FileKey(<redacted>)")
    }
}

/// The size of a complete blob file that holds `plain_len` bytes of plaintext: the header,
/// the plaintext and one tag per chunk (an empty blob has one empty chunk). A file of
/// another length was cut off or is not that blob.
pub fn sealed_len(plain_len: u64) -> u64 {
    let chunk = CHUNK_SIZE as u64;
    let chunks = plain_len.div_ceil(chunk).max(1);
    (HEADER_LEN as u64)
        .saturating_add(plain_len)
        .saturating_add(chunks.saturating_mul(format::TAG_LEN as u64))
}

/// Encrypts all of `data` into a new blob at `path` (atomic, 0600). Returns the plaintext length.
pub fn write_all(path: &std::path::Path, key: &FileKey, data: &[u8]) -> Result<u64, BlobError> {
    let mut w = BlobWriter::create(path, key)?;
    io::Write::write_all(&mut w, data).map_err(BlobError::from_io)?;
    w.finish()
}

/// Decrypts a whole blob into memory (zeroized on drop), verifying every chunk.
pub fn read_all(path: &std::path::Path, key: &FileKey) -> Result<Zeroizing<Vec<u8>>, BlobError> {
    let file = std::fs::File::open(path)?;
    // The plaintext is shorter than the file: reserving that much up front means `read_to_end`
    // never reallocates, so no unzeroized copy of the plaintext is left in freed memory.
    let capacity = file
        .metadata()?
        .len()
        .saturating_sub(format::HEADER_LEN as u64);
    let mut r = BlobReader::new(file, key)?;
    let mut out = Zeroizing::new(Vec::with_capacity(
        usize::try_from(capacity).map_err(|_| BlobError::TooLarge)?,
    ));
    io::Read::read_to_end(&mut r, &mut out).map_err(BlobError::from_io)?;
    Ok(out)
}
