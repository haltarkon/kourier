//! The `kourier-blob v1` header and the chunk cipher.

use crate::{BlobError, FileKey};
use aead_stream::{StreamBE32, StreamPrimitive};
use aes_gcm::{Aes256Gcm, KeyInit};

/// File magic.
pub const MAGIC: [u8; 8] = *b"KOURBLOB";
/// Format version written by this build.
pub const VERSION: u8 = 1;
/// Plaintext bytes per chunk.
pub const CHUNK_SIZE: usize = 64 * 1024;
/// Header length: magic, version, chunk size (u32 LE), nonce prefix.
pub const HEADER_LEN: usize = 8 + 1 + 4 + NONCE_PREFIX_LEN;
/// AES-GCM tag per chunk.
pub(crate) const TAG_LEN: usize = 16;

const NONCE_PREFIX_LEN: usize = 7;
/// Readers accept chunk sizes in this range, so a hostile header cannot demand huge buffers.
const CHUNK_RANGE: std::ops::RangeInclusive<usize> = 1024..=(16 * 1024 * 1024);

/// A parsed header plus the cipher for its chunks.
pub(crate) struct Sealer {
    pub(crate) header: [u8; HEADER_LEN],
    pub(crate) chunk_size: usize,
    stream: StreamBE32<Aes256Gcm>,
}

impl Sealer {
    /// A new header with a random nonce prefix.
    pub(crate) fn new(key: &FileKey) -> Self {
        use rand::RngCore;
        let mut prefix = [0u8; NONCE_PREFIX_LEN];
        rand::rng().fill_bytes(&mut prefix);
        let mut header = [0u8; HEADER_LEN];
        header[..8].copy_from_slice(&MAGIC);
        header[8] = VERSION;
        let chunk = u32::try_from(CHUNK_SIZE).unwrap_or(u32::MAX);
        header[9..13].copy_from_slice(&chunk.to_le_bytes());
        header[13..].copy_from_slice(&prefix);
        Self::with_header(key, header, CHUNK_SIZE)
    }

    /// Parses a header read from a file.
    pub(crate) fn parse(key: &FileKey, header: [u8; HEADER_LEN]) -> Result<Self, BlobError> {
        if header[..8] != MAGIC {
            return Err(BlobError::NotABlob);
        }
        if header[8] != VERSION {
            return Err(BlobError::UnsupportedVersion(header[8]));
        }
        let mut size = [0u8; 4];
        size.copy_from_slice(&header[9..13]);
        let chunk_size =
            usize::try_from(u32::from_le_bytes(size)).map_err(|_| BlobError::BadHeader)?;
        if !CHUNK_RANGE.contains(&chunk_size) {
            return Err(BlobError::BadHeader);
        }
        Ok(Self::with_header(key, header, chunk_size))
    }

    fn with_header(key: &FileKey, header: [u8; HEADER_LEN], chunk_size: usize) -> Self {
        let aead = Aes256Gcm::new(key.as_bytes().into());
        let mut prefix = [0u8; NONCE_PREFIX_LEN];
        prefix.copy_from_slice(&header[13..]);
        let stream = <StreamBE32<Aes256Gcm> as aead_stream::NewStream<Aes256Gcm>>::from_aead(
            aead,
            &prefix.into(),
        );
        Self {
            header,
            chunk_size,
            stream,
        }
    }

    /// Encrypts chunk `index` in place (appends the tag).
    pub(crate) fn seal(&self, index: u32, last: bool, buf: &mut Vec<u8>) -> Result<(), BlobError> {
        self.stream
            .encrypt_in_place(index, last, &self.header, buf)
            .map_err(|_| BlobError::TooLarge)
    }

    /// Decrypts chunk `index` in place (strips the tag).
    pub(crate) fn open(&self, index: u32, last: bool, buf: &mut Vec<u8>) -> Result<(), BlobError> {
        self.stream
            .decrypt_in_place(index, last, &self.header, buf)
            .map_err(|_| BlobError::Corrupt)
    }
}
