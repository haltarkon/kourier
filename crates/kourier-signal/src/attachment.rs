//! Receive-side attachments (§6.11): `AttachmentPointer` crypto and the CDN download.
//!
//! The format (`SD/ts/AttachmentCrypto.node.ts` `encryptAttachmentV2`, `decryptAttachmentV2ToSink`):
//! a 64-byte key = AES-256 key ‖ HMAC-SHA256 key; the blob is `IV(16) ‖ AES-256-CBC(PKCS7,
//! plaintext ‖ zero padding) ‖ HMAC-SHA256(IV ‖ ciphertext)`; the pointer's `digest` is SHA-256
//! of the whole blob, `size` the unpadded plaintext length. Decryption verifies the MAC, then the
//! plaintext hash when one is known, else the digest, and strips the padding to `size`.
//!
//! [`Decryptor`] streams: ciphertext goes in in any chunk sizes, plaintext comes out through a
//! sink as soon as it is decrypted (into a `kourier_blob` writer that only renames its file into
//! place once [`Decryptor::finish`] succeeded, so unverified plaintext never becomes a blob).
//! Small blobs (the contact-sync blob, §6.9) are decrypted in memory with [`decrypt_in_memory`].
//! Incremental MACs (video streaming) are not checked: the digest covers the whole blob anyway.
//!
//! [`Encryptor`] is the send side (`encryptAttachmentV2`): plaintext in, the blob out, padded to
//! Desktop's [`log_pad_size`] buckets, with the digest, the plaintext hash and, for `video/mp4`,
//! the incremental MAC over the blob (libsignal's `incremental_mac`, chunk size from the blob
//! length as Desktop's `inferChunkSize`).
//!
//! Downloads stream the CDN body through a channel into a blocking task that decrypts and writes
//! the `kourier-blob` file, so neither AES nor file I/O runs on the network runtime. A failed
//! download is reported by its cause: a broken transfer is worth another try, a blob that does
//! not verify, is longer than its pointer allows or lies on a CDN Kourier does not know is not.

use std::fmt;

use aes::Aes256Dec;
use aes::cipher::{BlockCipherDecrypt as _, KeyInit as _};
use hmac::{Hmac, Mac as _};
use sha2::{Digest as _, Sha256};
use zeroize::Zeroizing;

use crate::net::{HttpsHost, HttpsRequest, Net, SignalTransport};
use crate::proto;

/// Length of an attachment key (AES-256 key ‖ HMAC-SHA256 key).
pub(crate) const KEY_LEN: usize = 64;
/// AES block and IV length.
const BLOCK_LEN: usize = 16;
/// HMAC-SHA256 tag length.
const MAC_LEN: usize = 32;
/// The shortest valid blob: IV, one cipher block, MAC.
const MIN_BLOB_LEN: usize = BLOCK_LEN + BLOCK_LEN + MAC_LEN;

/// Why an attachment did not decrypt. Log-safe.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum AttachmentError {
    /// The pointer's key is not 64 bytes.
    #[error("attachment key must be 64 bytes")]
    BadKey,
    /// The pointer lacks what decryption needs (key, digest, size, CDN key).
    #[error("attachment pointer is incomplete: {0}")]
    Incomplete(&'static str),
    /// The blob is too short, or the ciphertext is not a whole number of blocks.
    #[error("attachment ciphertext has an invalid length")]
    BadLength,
    /// The MAC does not match: tampered, truncated, or the wrong key.
    #[error("attachment MAC does not match")]
    BadMac,
    /// The digest of the pointer does not match the blob.
    #[error("attachment digest does not match")]
    BadDigest,
    /// The known plaintext hash does not match.
    #[error("attachment plaintext hash does not match")]
    BadPlaintextHash,
    /// The PKCS7 padding is invalid.
    #[error("attachment padding is invalid")]
    BadPadding,
    /// The plaintext is shorter than the pointer's `size`.
    #[error("attachment is shorter than its stated size")]
    TooShort,
    /// The blob is larger than the limit for this download.
    #[error("attachment is larger than allowed")]
    TooLarge,
    /// The sink refused the plaintext (a full disk, a closed blob writer), or the plaintext
    /// source could not be read.
    #[error("writing the attachment failed: {0}")]
    Sink(String),
    /// The consumer of an encrypted stream went away (its upload request ended).
    #[error("the stream's consumer stopped")]
    Stopped,
}

/// The two keys of an attachment. `Debug` prints nothing secret.
pub(crate) struct AttachmentKeys {
    aes: Zeroizing<[u8; 32]>,
    mac: Zeroizing<[u8; 32]>,
}

impl AttachmentKeys {
    /// Splits a 64-byte pointer key.
    pub(crate) fn from_slice(key: &[u8]) -> Result<Self, AttachmentError> {
        if key.len() != KEY_LEN {
            return Err(AttachmentError::BadKey);
        }
        let mut aes = Zeroizing::new([0u8; 32]);
        let mut mac = Zeroizing::new([0u8; 32]);
        aes.copy_from_slice(&key[..32]);
        mac.copy_from_slice(&key[32..]);
        Ok(Self { aes, mac })
    }
}

impl fmt::Debug for AttachmentKeys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AttachmentKeys(<redacted>)")
    }
}

/// What the decrypted attachment is checked against (`IntegrityType` in Desktop).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Integrity {
    /// SHA-256 of the whole encrypted blob (the pointer's `digest`).
    Digest([u8; 32]),
    /// SHA-256 of the unpadded plaintext (known locally, e.g. from a backup).
    #[allow(dead_code, reason = "backup restores and re-downloads (after M1)")]
    PlaintextHash([u8; 32]),
}

/// What a successful decryption verified.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Verified {
    /// Plaintext bytes written (the pointer's `size`).
    pub(crate) size: u64,
    /// SHA-256 of the plaintext.
    pub(crate) plaintext_hash: [u8; 32],
}

/// Streaming AES-256-CBC + HMAC-SHA256 decryption of one attachment blob.
pub(crate) struct Decryptor {
    cipher: Aes256Dec,
    mac: Hmac<Sha256>,
    digest: Sha256,
    plaintext_hash: Sha256,
    integrity: Integrity,
    /// The pointer's `size`: plaintext beyond it is padding.
    size: u64,
    /// Most ciphertext bytes accepted.
    max_len: u64,
    /// Ciphertext bytes seen.
    seen: u64,
    /// Plaintext bytes emitted.
    written: u64,
    /// The last [`MAC_LEN`] bytes seen: the MAC, once the stream ends.
    tail: Vec<u8>,
    /// MAC-covered bytes not yet decrypted (less than one block after each update).
    pending: Vec<u8>,
    /// The previous cipher block (the IV at first); `None` until the IV arrived.
    previous: Option<[u8; BLOCK_LEN]>,
    /// The newest decrypted block, held back for the PKCS7 padding.
    last_plain: Option<Zeroizing<[u8; BLOCK_LEN]>>,
}

impl fmt::Debug for Decryptor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Decryptor")
            .field("size", &self.size)
            .field("seen", &self.seen)
            .finish_non_exhaustive()
    }
}

impl Decryptor {
    /// A decryptor for a blob whose plaintext is `size` bytes, refusing blobs longer than
    /// `max_len` bytes.
    pub(crate) fn new(
        keys: &AttachmentKeys,
        integrity: Integrity,
        size: u64,
        max_len: u64,
    ) -> Result<Self, AttachmentError> {
        let mac = Hmac::<Sha256>::new_from_slice(keys.mac.as_slice())
            .map_err(|_| AttachmentError::BadKey)?;
        Ok(Self {
            cipher: Aes256Dec::new((&*keys.aes).into()),
            mac,
            digest: Sha256::new(),
            plaintext_hash: Sha256::new(),
            integrity,
            size,
            max_len,
            seen: 0,
            written: 0,
            tail: Vec::with_capacity(2 * MAC_LEN),
            pending: Vec::with_capacity(2 * BLOCK_LEN),
            previous: None,
            last_plain: None,
        })
    }

    /// Feeds ciphertext; decrypted plaintext (up to `size`) goes to `sink`.
    pub(crate) fn update(
        &mut self,
        data: &[u8],
        sink: &mut dyn FnMut(&[u8]) -> Result<(), AttachmentError>,
    ) -> Result<(), AttachmentError> {
        self.seen = self.seen.saturating_add(data.len() as u64);
        if self.seen > self.max_len {
            return Err(AttachmentError::TooLarge);
        }
        self.digest.update(data);
        self.tail.extend_from_slice(data);
        if self.tail.len() <= MAC_LEN {
            return Ok(());
        }
        // Everything before the last MAC_LEN bytes is IV or ciphertext.
        let ready = self.tail.len() - MAC_LEN;
        self.mac.update(&self.tail[..ready]);
        self.pending.extend_from_slice(&self.tail[..ready]);
        self.tail.drain(..ready);
        self.decrypt_pending(sink)
    }

    fn decrypt_pending(
        &mut self,
        sink: &mut dyn FnMut(&[u8]) -> Result<(), AttachmentError>,
    ) -> Result<(), AttachmentError> {
        let mut offset = 0;
        while self.pending.len() - offset >= BLOCK_LEN {
            let mut block = [0u8; BLOCK_LEN];
            block.copy_from_slice(&self.pending[offset..offset + BLOCK_LEN]);
            offset += BLOCK_LEN;
            let Some(previous) = self.previous else {
                self.previous = Some(block);
                continue;
            };
            let mut plain = Zeroizing::new(block);
            self.cipher.decrypt_block((&mut *plain).into());
            for (p, c) in plain.iter_mut().zip(previous) {
                *p ^= c;
            }
            self.previous = Some(block);
            if let Some(ready) = self.last_plain.replace(plain) {
                self.emit(&ready[..], sink)?;
            }
        }
        self.pending.drain(..offset);
        Ok(())
    }

    /// Writes plaintext up to `size`; what lies beyond is padding.
    fn emit(
        &mut self,
        plain: &[u8],
        sink: &mut dyn FnMut(&[u8]) -> Result<(), AttachmentError>,
    ) -> Result<(), AttachmentError> {
        let left = self.size.saturating_sub(self.written);
        let n = usize::try_from(left).unwrap_or(usize::MAX).min(plain.len());
        if n > 0 {
            self.plaintext_hash.update(&plain[..n]);
            sink(&plain[..n])?;
            self.written += n as u64;
        }
        Ok(())
    }

    /// Ends the stream: checks length, MAC, padding, size and integrity. Only an `Ok` makes the
    /// plaintext written to `sink` trustworthy.
    pub(crate) fn finish(
        mut self,
        sink: &mut dyn FnMut(&[u8]) -> Result<(), AttachmentError>,
    ) -> Result<Verified, AttachmentError> {
        if self.seen < MIN_BLOB_LEN as u64 || !self.pending.is_empty() || self.tail.len() != MAC_LEN
        {
            return Err(AttachmentError::BadLength);
        }
        let their_mac = std::mem::take(&mut self.tail);
        self.mac
            .clone()
            .verify_slice(&their_mac)
            .map_err(|_| AttachmentError::BadMac)?;
        let last = self.last_plain.take().ok_or(AttachmentError::BadLength)?;
        let pad = usize::from(last[BLOCK_LEN - 1]);
        if pad == 0
            || pad > BLOCK_LEN
            || last[BLOCK_LEN - pad..]
                .iter()
                .any(|b| usize::from(*b) != pad)
        {
            return Err(AttachmentError::BadPadding);
        }
        self.emit(&last[..BLOCK_LEN - pad], sink)?;
        if self.written < self.size {
            return Err(AttachmentError::TooShort);
        }
        let plaintext_hash: [u8; 32] = self.plaintext_hash.finalize().into();
        match self.integrity {
            Integrity::Digest(expected) => {
                let digest: [u8; 32] = self.digest.finalize().into();
                if digest != expected {
                    return Err(AttachmentError::BadDigest);
                }
            }
            Integrity::PlaintextHash(expected) => {
                if plaintext_hash != expected {
                    return Err(AttachmentError::BadPlaintextHash);
                }
            }
        }
        Ok(Verified {
            size: self.written,
            plaintext_hash,
        })
    }
}

/// Decrypts a whole blob in memory (zeroized on drop); `max_len` bounds the ciphertext.
pub(crate) fn decrypt_in_memory(
    keys: &AttachmentKeys,
    integrity: Integrity,
    size: u64,
    blob: &[u8],
    max_len: u64,
) -> Result<Zeroizing<Vec<u8>>, AttachmentError> {
    let capacity = usize::try_from(size.min(blob.len() as u64)).unwrap_or(0);
    let mut out = Zeroizing::new(Vec::with_capacity(capacity));
    let mut decryptor = Decryptor::new(keys, integrity, size, max_len)?;
    let mut sink = |plain: &[u8]| {
        out.extend_from_slice(plain);
        Ok(())
    };
    decryptor.update(blob, &mut sink)?;
    decryptor.finish(&mut sink)?;
    Ok(out)
}

/// Desktop's `logPadSize` (`SD/ts/util/logPadSize.std.ts`): the padded plaintext length.
pub(crate) fn log_pad_size(size: u64) -> u64 {
    let exponent = ((size as f64).ln() / 1.05f64.ln()).ceil();
    (1.05f64.powf(exponent).floor() as u64).max(541)
}

/// The longest blob a plaintext of `size` bytes can have: IV, the padded plaintext rounded up
/// to whole blocks (PKCS7 always adds one), MAC.
pub(crate) fn max_blob_len(size: u64) -> u64 {
    let padded = log_pad_size(size).max(size);
    let blocks = padded / BLOCK_LEN as u64 + 1;
    (BLOCK_LEN as u64)
        .saturating_add(blocks.saturating_mul(BLOCK_LEN as u64))
        .saturating_add(MAC_LEN as u64)
}

/// `video/mp4` carries an incremental MAC (`SD/ts/types/MIME.std.ts` `supportsIncrementalMac`).
pub(crate) fn supports_incremental_mac(mime: &str) -> bool {
    mime.eq_ignore_ascii_case("video/mp4")
}

/// What [`Encryptor::finish`] computed over the blob it produced.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Encrypted {
    /// SHA-256 of the whole blob (the pointer's `digest`).
    pub(crate) digest: [u8; 32],
    /// SHA-256 of the unpadded plaintext.
    pub(crate) plaintext_hash: [u8; 32],
    /// The chunk MACs over the blob (`incrementalMac`), for `video/mp4`.
    pub(crate) incremental_mac: Option<Vec<u8>>,
    /// The chunk size of `incremental_mac` (`chunkSize`).
    pub(crate) chunk_size: Option<u32>,
    /// Blob bytes produced.
    pub(crate) len: u64,
}

impl fmt::Debug for Encrypted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Encrypted")
            .field("len", &self.len)
            .field("chunk_size", &self.chunk_size)
            .finish_non_exhaustive()
    }
}

/// Zero bytes fed through the cipher for the padding, a slice at a time.
const ZEROS: [u8; 4096] = [0; 4096];

/// Streaming AES-256-CBC + HMAC-SHA256 encryption of one attachment (`encryptAttachmentV2`):
/// `IV ‖ CBC(plaintext ‖ zeros to log_pad_size ‖ PKCS7) ‖ HMAC(IV ‖ ciphertext)`. The plaintext
/// length is declared up front (the upload needs the blob length before the first byte); feeding
/// more or less is an error.
pub(crate) struct Encryptor {
    cipher: aes::Aes256Enc,
    mac: Hmac<Sha256>,
    digest: Sha256,
    plaintext_hash: Sha256,
    incremental: Option<libsignal_protocol::incremental_mac::Incremental<Hmac<Sha256>>>,
    macs: Vec<u8>,
    chunk_size: usize,
    /// The declared plaintext length.
    size: u64,
    /// Plaintext bytes taken.
    taken: u64,
    /// The IV until it was written out.
    iv: Option<[u8; BLOCK_LEN]>,
    /// The previous cipher block (CBC chaining).
    previous: [u8; BLOCK_LEN],
    /// Plaintext short of a whole block.
    pending: Zeroizing<Vec<u8>>,
    /// Blob bytes produced.
    produced: u64,
}

impl fmt::Debug for Encryptor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Encryptor")
            .field("size", &self.size)
            .field("taken", &self.taken)
            .finish_non_exhaustive()
    }
}

impl Encryptor {
    /// An encryptor for `size` plaintext bytes under `keys` and `iv` (random per upload; a
    /// resumed upload replays the same key and IV). `incremental_mac` adds the chunk MACs.
    pub(crate) fn new(
        keys: &AttachmentKeys,
        iv: [u8; BLOCK_LEN],
        size: u64,
        incremental_mac: bool,
    ) -> Result<Self, AttachmentError> {
        use aes::cipher::KeyInit as _;
        let mac = Hmac::<Sha256>::new_from_slice(keys.mac.as_slice())
            .map_err(|_| AttachmentError::BadKey)?;
        let blob_len =
            usize::try_from(max_blob_len(size)).map_err(|_| AttachmentError::TooLarge)?;
        let chunk_size =
            libsignal_protocol::incremental_mac::calculate_chunk_size::<Sha256>(blob_len);
        let incremental = incremental_mac.then(|| {
            libsignal_protocol::incremental_mac::Incremental::new(mac.clone(), chunk_size)
        });
        Ok(Self {
            cipher: aes::Aes256Enc::new((&*keys.aes).into()),
            mac,
            digest: Sha256::new(),
            plaintext_hash: Sha256::new(),
            incremental,
            macs: Vec::new(),
            chunk_size,
            size,
            taken: 0,
            iv: Some(iv),
            previous: iv,
            pending: Zeroizing::new(Vec::with_capacity(BLOCK_LEN)),
            produced: 0,
        })
    }

    /// Encrypts `plain`; blob bytes are appended to `out`.
    pub(crate) fn update(
        &mut self,
        plain: &[u8],
        out: &mut Vec<u8>,
    ) -> Result<(), AttachmentError> {
        let len = plain.len() as u64;
        if self.taken.saturating_add(len) > self.size {
            return Err(AttachmentError::TooLarge);
        }
        self.taken += len;
        self.plaintext_hash.update(plain);
        self.encrypt(plain, out);
        Ok(())
    }

    /// Ends the blob: zero padding to the bucket, PKCS7, the MAC. Fails when less plaintext
    /// than declared came in.
    pub(crate) fn finish(mut self, out: &mut Vec<u8>) -> Result<Encrypted, AttachmentError> {
        if self.taken != self.size {
            return Err(AttachmentError::TooShort);
        }
        let mut zeros = log_pad_size(self.size).saturating_sub(self.size);
        while zeros > 0 {
            let n = usize::try_from(zeros)
                .unwrap_or(ZEROS.len())
                .min(ZEROS.len());
            self.encrypt(&ZEROS[..n], out);
            zeros -= n as u64;
        }
        let pad = BLOCK_LEN - self.pending.len();
        let pkcs = vec![u8::try_from(pad).unwrap_or(16); pad];
        self.encrypt(&pkcs, out);
        debug_assert!(self.pending.is_empty());
        let mac = self.mac.clone().finalize().into_bytes();
        let start = out.len();
        out.extend_from_slice(&mac);
        self.track(&out[start..]);
        let incremental_mac = self.incremental.take().map(|incremental| {
            let mut macs = std::mem::take(&mut self.macs);
            macs.extend_from_slice(&incremental.finalize());
            macs
        });
        Ok(Encrypted {
            digest: self.digest.finalize().into(),
            plaintext_hash: self.plaintext_hash.finalize().into(),
            chunk_size: incremental_mac
                .as_ref()
                .map(|_| u32::try_from(self.chunk_size).unwrap_or(u32::MAX)),
            incremental_mac,
            len: self.produced,
        })
    }

    /// Writes the IV once, then CBC-encrypts every whole block of `pending ‖ plain` into `out`.
    fn encrypt(&mut self, mut plain: &[u8], out: &mut Vec<u8>) {
        let start = out.len();
        if let Some(iv) = self.iv.take() {
            out.extend_from_slice(&iv);
        }
        if !self.pending.is_empty() {
            let take = (BLOCK_LEN - self.pending.len()).min(plain.len());
            self.pending.extend_from_slice(&plain[..take]);
            plain = &plain[take..];
            if self.pending.len() == BLOCK_LEN {
                let mut block = [0u8; BLOCK_LEN];
                block.copy_from_slice(&self.pending);
                self.pending.clear();
                self.chain(&mut block);
                out.extend_from_slice(&block);
            }
        }
        let (blocks, rest) = plain.as_chunks::<BLOCK_LEN>();
        for chunk in blocks {
            let mut block = *chunk;
            self.chain(&mut block);
            out.extend_from_slice(&block);
        }
        self.pending.extend_from_slice(rest);
        // IV and cipher blocks are covered by the MAC; everything by the digests.
        self.mac.update(&out[start..]);
        self.track(&out[start..]);
    }

    /// Feeds produced blob bytes to the digest and the incremental MAC.
    fn track(&mut self, bytes: &[u8]) {
        self.produced += bytes.len() as u64;
        self.digest.update(bytes);
        if let Some(incremental) = self.incremental.as_mut() {
            for mac in incremental.update(bytes) {
                self.macs.extend_from_slice(&mac);
            }
        }
    }

    /// `block ^= previous; encrypt; previous = block`.
    fn chain(&mut self, block: &mut [u8; BLOCK_LEN]) {
        use aes::cipher::BlockCipherEncrypt as _;
        for (b, p) in block.iter_mut().zip(self.previous) {
            *b ^= p;
        }
        self.cipher.encrypt_block(block.into());
        self.previous = *block;
    }
}

/// What the receive path needs from an `AttachmentPointer`.
#[derive(Clone)]
pub(crate) struct Pointer {
    /// CDN number (0 when absent).
    pub(crate) cdn: u32,
    /// The CDN key (`cdnKey`, or the legacy numeric `cdnId`).
    pub(crate) cdn_key: String,
    /// The 64-byte key.
    pub(crate) key: Zeroizing<Vec<u8>>,
    /// The blob's SHA-256.
    pub(crate) digest: [u8; 32],
    /// Unpadded plaintext length.
    pub(crate) size: u64,
    /// MIME type, as the sender says.
    pub(crate) content_type: Option<String>,
    /// Width and height, as the sender says.
    pub(crate) dims: Option<(u32, u32)>,
}

impl fmt::Debug for Pointer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pointer")
            .field("cdn", &self.cdn)
            .field("size", &self.size)
            .finish_non_exhaustive()
    }
}

impl Pointer {
    /// Checks a pointer from the wire.
    pub(crate) fn from_proto(pointer: &proto::AttachmentPointer) -> Result<Self, AttachmentError> {
        use proto::attachment_pointer::AttachmentIdentifier;
        let cdn_key = match &pointer.attachment_identifier {
            Some(AttachmentIdentifier::CdnKey(key)) if valid_cdn_key(key) => key.clone(),
            Some(AttachmentIdentifier::CdnId(id)) if *id != 0 => id.to_string(),
            _ => return Err(AttachmentError::Incomplete("CDN key")),
        };
        let key = pointer
            .key
            .as_deref()
            .filter(|key| key.len() == KEY_LEN)
            .ok_or(AttachmentError::Incomplete("key"))?;
        let digest = pointer
            .digest
            .as_deref()
            .and_then(|d| <[u8; 32]>::try_from(d).ok())
            .ok_or(AttachmentError::Incomplete("digest"))?;
        let size = pointer.size.ok_or(AttachmentError::Incomplete("size"))?;
        Ok(Self {
            cdn: pointer.cdn_number.unwrap_or(0),
            cdn_key,
            key: Zeroizing::new(key.to_vec()),
            digest,
            size: u64::from(size),
            content_type: pointer
                .content_type
                .clone()
                .filter(|t| !t.is_empty() && t.len() <= 255),
            dims: pointer
                .width
                .zip(pointer.height)
                .filter(|(w, h)| *w > 0 && *h > 0),
        })
    }

    /// The pointer stored as an attachment's `remote` ([`POINTER_BLOB_V`]).
    pub(crate) fn from_blob(remote: &kourier_model::BackendBlob) -> Result<Self, AttachmentError> {
        if remote.v != POINTER_BLOB_V {
            return Err(AttachmentError::Incomplete("pointer format"));
        }
        let pointer = <proto::AttachmentPointer as prost::Message>::decode(&remote.bytes[..])
            .map_err(|_| AttachmentError::Incomplete("pointer"))?;
        Self::from_proto(&pointer)
    }

    /// `GET {cdn[N]}/attachments/{cdnKey}` (`WebAPI.preload.ts` `getAttachment`).
    pub(crate) fn request(&self, timeout: std::time::Duration) -> HttpsRequest {
        let mut request = HttpsRequest::get(
            HttpsHost::Cdn(self.cdn),
            format!("/attachments/{}", self.cdn_key),
        );
        request.timeout = timeout;
        request
    }
}

/// `BackendBlob.v` of an attachment's `remote`: the encoded `AttachmentPointer` (v 1 is the raw
/// `DataMessage` of `Body::Unsupported`).
pub(crate) const POINTER_BLOB_V: u16 = 2;

/// CDN keys are URL-safe tokens; anything else could escape the path.
pub(crate) fn valid_cdn_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 256
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'='))
        && key != "."
        && key != ".."
}

/// Why a download did not produce a verified attachment.
#[derive(Debug, thiserror::Error)]
pub(crate) enum DownloadError {
    /// The CDN could not be reached or did not answer.
    #[error(transparent)]
    Transport(#[from] crate::net::TransportError),
    /// The CDN answered with this status (404: expired or never uploaded).
    #[error("CDN answered {0}")]
    Status(u16),
    /// The blob failed verification.
    #[error(transparent)]
    Crypto(#[from] AttachmentError),
}

impl From<DownloadError> for crate::error::Error {
    fn from(e: DownloadError) -> Self {
        match e {
            DownloadError::Transport(e) => Self::Transport(e),
            DownloadError::Status(status) => Self::Status {
                what: "attachment download",
                status,
            },
            DownloadError::Crypto(e) => Self::Decode(match e {
                AttachmentError::BadKey | AttachmentError::Incomplete(_) => "attachment pointer",
                AttachmentError::TooLarge => "attachment (too large)",
                AttachmentError::BadMac => "attachment (MAC)",
                AttachmentError::BadDigest | AttachmentError::BadPlaintextHash => {
                    "attachment (digest)"
                }
                _ => "attachment",
            }),
        }
    }
}

/// Downloads `pointer` from its CDN and decrypts it in memory, refusing blobs longer than
/// `max_len` bytes (used for the contact-sync blob).
pub(crate) async fn download_in_memory<T: SignalTransport>(
    net: &Net<T>,
    pointer: &Pointer,
    max_len: u64,
    timeout: std::time::Duration,
) -> Result<Zeroizing<Vec<u8>>, DownloadError> {
    let mut request = pointer.request(timeout);
    request.max_body = max_len;
    let transport = net.clone();
    let response = transport.https(request).await?;
    if !response.status.is_success() {
        return Err(DownloadError::Status(response.status.as_u16()));
    }
    let keys = AttachmentKeys::from_slice(&pointer.key)?;
    Ok(decrypt_in_memory(
        &keys,
        Integrity::Digest(pointer.digest),
        pointer.size,
        &response.body,
        max_len,
    )?)
}

/// How a download goes on after its transfer broke off.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ResumePolicy {
    /// Requests in a row that bring no further byte before the download fails (the core
    /// resends the command later, from the start).
    pub(crate) fruitless: u32,
    /// The wait before a new request after one that brought nothing; doubled each time, at
    /// most [`MAX_RESUME_PAUSE`].
    pub(crate) pause: std::time::Duration,
}

impl ResumePolicy {
    /// Five fruitless requests with 1, 2, 4 and 8 seconds between them: a network change or a
    /// dropped connection is ridden out, a CDN that is really gone fails within a minute.
    pub(crate) const DEFAULT: Self = Self {
        fruitless: 5,
        pause: std::time::Duration::from_secs(1),
    };
}

/// Longest wait between two requests of one download.
const MAX_RESUME_PAUSE: std::time::Duration = std::time::Duration::from_secs(8);
/// Most requests of one download (each must bring at least one byte to count as progress, so
/// this only bounds a server that feeds a download byte by byte).
const MAX_DOWNLOAD_REQUESTS: u32 = 200;

/// Whether a transfer that ended with `error` may go on with a range request: the network or
/// the CDN failed, not the blob or the request.
fn resumable(error: &crate::net::TransportError) -> bool {
    use crate::net::TransportError;
    matches!(
        error,
        TransportError::Timeout
            | TransportError::Disconnected
            | TransportError::Unreachable(_)
            | TransportError::Https(_)
            | TransportError::ServerSide
    )
}

/// Streams the blob of `pointer` into `sink`, going on with `Range` requests where a transfer
/// broke off (`HttpsClient::download`), until the body is complete, the CDN refuses, or
/// `policy` gives up. A download whose first request brings nothing fails as it did before
/// resuming existed: there is nothing to resume. A resumed request the CDN answers with 416
/// (no byte from there on) ends the body as complete.
async fn fetch<T: SignalTransport>(
    net: &Net<T>,
    pointer: &Pointer,
    sink: &crate::net::DownloadSink,
    max_len: u64,
    timeout: std::time::Duration,
    policy: ResumePolicy,
) -> Result<http::StatusCode, crate::net::TransportError> {
    let request = || {
        let mut request = pointer.request(timeout);
        request.max_body = max_len;
        request
    };
    fetch_blob(net, &request, sink, policy).await
}

/// [`fetch`] for any blob of a CDN: `request` builds the `GET` (without a range; the sink
/// adds it). The history transfer's archive comes through here too (`transfer.rs`), with a
/// sink that may start behind byte 0 after a restart.
pub(crate) async fn fetch_blob<T: SignalTransport>(
    net: &Net<T>,
    request: &dyn Fn() -> HttpsRequest,
    sink: &crate::net::DownloadSink,
    policy: ResumePolicy,
) -> Result<http::StatusCode, crate::net::TransportError> {
    let mut fruitless = 0u32;
    let mut requests = 0u32;
    loop {
        let before = sink.taken();
        let request = request();
        requests += 1;
        let outcome = net.https_download(request, sink.resume()).await;
        match &outcome {
            Ok(status) if status.is_success() => return outcome,
            // The CDN has nothing from that byte on: the transfer broke off after the last
            // byte of the blob, before its clean end. What arrived is all there is, and the
            // writer's verification of the whole stream says whether it is the blob. (As a
            // refusal this would end a download that holds every byte as a failure for
            // good.)
            Ok(status) if before > 0 && *status == http::StatusCode::RANGE_NOT_SATISFIABLE => {
                tracing::debug!(
                    have = before,
                    "nothing left to resume: the blob is complete"
                );
                return Ok(http::StatusCode::PARTIAL_CONTENT);
            }
            // The rest of a blob we hold a part of, refused for now: tried again like a
            // transfer that broke off. Any other refusal is the CDN's answer.
            Ok(status) if before > 0 && (status.is_server_error() || status.as_u16() == 429) => {}
            Ok(_) => return outcome,
            Err(e) if resumable(e) => {}
            Err(_) => return outcome,
        }
        let have = sink.taken();
        fruitless = if have > before { 0 } else { fruitless + 1 };
        if have == 0 || fruitless >= policy.fruitless || requests >= MAX_DOWNLOAD_REQUESTS {
            return outcome;
        }
        tracing::info!(
            have,
            fruitless,
            requests,
            "attachment download broke off; going on from where it stopped"
        );
        if fruitless > 0 {
            let pause = policy
                .pause
                .saturating_mul(1u32 << (fruitless - 1).min(16))
                .min(MAX_RESUME_PAUSE);
            tokio::time::sleep(pause).await;
        }
    }
}

/// Downloads `pointer` and streams its verified plaintext into a new `kourier-blob` file at
/// `dest` under `key` (the core's `BlobTarget`). The file appears only once the MAC, padding,
/// size and digest checked out; any failure leaves no file. `limit` bounds the plaintext
/// (`global.attachments.maxReceiveBytes`). The body travels through a bounded channel into a
/// blocking task that decrypts and writes, so the network runtime only moves bytes.
///
/// A transfer that breaks off goes on with a range request from the byte it stopped at
/// ([`fetch`], `policy`); the decryption runs over the one unbroken stream the requests add
/// up to, so the MAC and the digest still verify the blob as a whole at the end. `written`
/// counts the plaintext bytes written so far (the progress the user sees).
#[allow(
    clippy::too_many_arguments,
    reason = "the pointer, the target and how to transfer"
)]
pub(crate) async fn download_to_blob<T: SignalTransport>(
    net: &Net<T>,
    pointer: &Pointer,
    dest: &std::path::Path,
    key: &kourier_model::BlobKey,
    limit: u64,
    timeout: std::time::Duration,
    policy: ResumePolicy,
    written: &std::sync::Arc<std::sync::atomic::AtomicU64>,
) -> Result<kourier_model::AttachmentMeta, DownloadError> {
    if pointer.size > limit {
        return Err(AttachmentError::TooLarge.into());
    }
    let keys = AttachmentKeys::from_slice(&pointer.key)?;
    let max_len = max_blob_len(pointer.size);
    let (tx, rx) = tokio::sync::mpsc::channel(DOWNLOAD_QUEUE);
    let sink = crate::net::DownloadSink::new(tx);
    let target = BlobFile {
        path: dest.to_owned(),
        key: kourier_blob::FileKey::from_bytes(*key.expose()),
    };
    let (digest, size) = (pointer.digest, pointer.size);
    let counter = written.clone();
    let writer = tokio::task::spawn_blocking(move || {
        write_verified(
            rx,
            &target,
            &keys,
            Integrity::Digest(digest),
            size,
            max_len,
            &counter,
        )
    });
    let status = fetch(net, pointer, &sink, max_len, timeout, policy).await;
    // The writer ends its stream when the last sender is gone.
    drop(sink);
    let written = writer
        .await
        .map_err(|_| AttachmentError::Sink("the writer task stopped".to_owned()))
        .and_then(|result| result);
    let verified = match (status, written) {
        (Ok(status), _) if !status.is_success() => {
            return Err(DownloadError::Status(status.as_u16()));
        }
        (Ok(_), written) => written?,
        // The writer refused the stream (too large, a full disk): that stopped the download.
        (Err(_), Err(e @ (AttachmentError::TooLarge | AttachmentError::Sink(_)))) => {
            return Err(e.into());
        }
        // The transfer broke off. Whatever the writer made of the cut stream (a bad length, or
        // a "bad MAC" when the cut fell on a block boundary) is not the reason.
        (Err(e), _) => return Err(e.into()),
    };
    Ok(kourier_model::AttachmentMeta {
        size: verified.size,
        mime: pointer.content_type.clone(),
        dims: pointer.dims,
        duration_ms: None,
    })
}

/// Downloaded chunks waiting for the writer (a slow disk holds the download back).
const DOWNLOAD_QUEUE: usize = 16;

/// Where a verified download goes.
struct BlobFile {
    path: std::path::PathBuf,
    key: kourier_blob::FileKey,
}

/// Blocking: decrypts the chunks from `rx` into a new blob file at `target`, which appears
/// only once everything verified. Plaintext bytes written are added to `written`.
fn write_verified(
    mut rx: tokio::sync::mpsc::Receiver<bytes::Bytes>,
    target: &BlobFile,
    keys: &AttachmentKeys,
    integrity: Integrity,
    size: u64,
    max_len: u64,
    written: &std::sync::atomic::AtomicU64,
) -> Result<Verified, AttachmentError> {
    use std::io::Write as _;
    use std::os::unix::fs::DirBuilderExt as _;
    let sink_error = |e: std::io::Error| AttachmentError::Sink(e.kind().to_string());
    if let Some(parent) = target.path.parent() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)
            .map_err(sink_error)?;
    }
    let mut writer = kourier_blob::BlobWriter::create(&target.path, &target.key)
        .map_err(|e| AttachmentError::Sink(e.to_string()))?;
    let mut decryptor = Decryptor::new(keys, integrity, size, max_len)?;
    let mut write = |plain: &[u8]| {
        writer.write_all(plain).map_err(sink_error)?;
        written.fetch_add(plain.len() as u64, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    };
    while let Some(chunk) = rx.blocking_recv() {
        decryptor.update(&chunk, &mut write)?;
    }
    let verified = decryptor.finish(&mut write)?;
    writer
        .finish()
        .map_err(|e| AttachmentError::Sink(e.to_string()))?;
    Ok(verified)
}

/// Receive limit without a remote config (Desktop: 1.25 × `global.attachments.maxBytes`).
pub(crate) const DEFAULT_MAX_RECEIVE_BYTES: u64 = 125 * 1024 * 1024;
/// Shortest whole-request timeout of a request that carries a blob.
const MIN_TRANSFER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10 * 60);
/// The slowest link a blob transfer is given time for, in bytes per second.
const MIN_TRANSFER_RATE: u64 = 16 * 1024;

/// The whole-request timeout of a request that carries `len` blob bytes: ten minutes, and
/// longer for a blob that needs longer at [`MIN_TRANSFER_RATE`] (100 MiB: 107 minutes), so a
/// large file on a slow link is not cut off every time before it can finish.
pub(crate) fn transfer_timeout(len: u64) -> std::time::Duration {
    std::time::Duration::from_secs(len / MIN_TRANSFER_RATE).max(MIN_TRANSFER_TIMEOUT)
}

/// How often download progress goes to the core.
const PROGRESS_EVERY: std::time::Duration = std::time::Duration::from_millis(250);

/// Sends `DownloadProgress` every [`PROGRESS_EVERY`] while `written` grows (aborted when the
/// download ends). It never reports the whole size: that event follows the verification.
async fn report_progress<T: SignalTransport>(
    shared: std::rc::Rc<crate::actor::Shared<T>>,
    attachment: kourier_model::AttachmentId,
    written: std::sync::Arc<std::sync::atomic::AtomicU64>,
    total: u64,
) {
    let mut last = 0u64;
    loop {
        tokio::time::sleep(PROGRESS_EVERY).await;
        let done = written
            .load(std::sync::atomic::Ordering::Relaxed)
            .min(total.saturating_sub(1));
        if done == last {
            continue;
        }
        last = done;
        let progress = kourier_backend_api::BackendEvent::DownloadProgress {
            attachment,
            done,
            total,
        };
        if shared.emit(progress).await.is_err() {
            return;
        }
    }
}

/// `Command::DownloadAttachment` (§5.8): progress is ephemeral, the result is durable and goes
/// through `core_outbox` only once the file is complete (the blob writer renamed and synced it).
/// Progress follows the plaintext bytes written, from 0 to the size once the blob verified; a
/// transfer that broke off goes on where it stopped ([`download_to_blob`]).
pub(crate) async fn run_command<T: SignalTransport>(
    shared: std::rc::Rc<crate::actor::Shared<T>>,
    attachment: kourier_model::AttachmentId,
    request: kourier_model::DownloadId,
    remote: kourier_model::BackendBlob,
    dest: kourier_backend_api::BlobTarget,
) {
    use kourier_backend_api::BackendEvent;

    let limit = shared
        .db
        .read(crate::maintenance::load_remote_config)
        .ok()
        .and_then(|config| crate::maintenance::account_caps(&config).max_receive_attachment_bytes)
        .unwrap_or(DEFAULT_MAX_RECEIVE_BYTES);
    let result = match Pointer::from_blob(&remote) {
        Ok(pointer) => {
            let _ = shared
                .emit(BackendEvent::DownloadProgress {
                    attachment,
                    done: 0,
                    total: pointer.size,
                })
                .await;
            let written = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
            let ticker = tokio::task::spawn_local(report_progress(
                shared.clone(),
                attachment,
                written.clone(),
                pointer.size,
            ));
            let meta = download_to_blob(
                &shared.net,
                &pointer,
                &dest.path,
                &dest.key,
                limit,
                transfer_timeout(max_blob_len(pointer.size)),
                shared.tuning.download_resume,
                &written,
            )
            .await
            .map_err(|e| {
                tracing::warn!(error = %e, "attachment download failed");
                e.to_backend_error()
            });
            ticker.abort();
            if let Ok(meta) = &meta {
                let _ = shared
                    .emit(BackendEvent::DownloadProgress {
                        attachment,
                        done: meta.size,
                        total: meta.size,
                    })
                    .await;
            }
            meta
        }
        Err(e) => Err(kourier_model::BackendError::new(
            kourier_model::ErrorKind::InvalidInput,
            e.to_string(),
        )),
    };
    let now = shared.now();
    let events = [BackendEvent::AttachmentDone {
        attachment,
        request: Some(request),
        result,
    }];
    if let Err(e) = shared
        .db
        .zone(|tx| crate::outbox::append_events(tx, &events, now, &shared.wake))
    {
        tracing::error!(error = %e, "recording a finished download failed");
    }
}

impl DownloadError {
    /// The contract error of a failed `DownloadAttachment` (`AttachmentDone`).
    pub(crate) fn to_backend_error(&self) -> kourier_model::BackendError {
        use kourier_model::{BackendError, ErrorKind};
        match self {
            // A blob longer than its pointer allows, however the stream was stopped.
            Self::Transport(crate::net::TransportError::BodyTooLarge)
            | Self::Status(413)
            | Self::Crypto(AttachmentError::TooLarge) => {
                BackendError::new(ErrorKind::TooLarge, "the attachment is too large")
            }
            // The pointer names a place no request can be made for (a CDN Kourier does not
            // know): trying again cannot help.
            Self::Transport(e @ crate::net::TransportError::InvalidRequest(_)) => {
                BackendError::new(ErrorKind::InvalidInput, e.to_string())
            }
            Self::Transport(e) => BackendError::transient(e.to_string()),
            Self::Status(404 | 410) => {
                BackendError::new(ErrorKind::Expired, "the attachment is no longer on the CDN")
            }
            Self::Status(status) if *status >= 500 || *status == 429 => {
                BackendError::transient(format!("CDN answered {status}"))
            }
            Self::Status(status) => {
                BackendError::new(ErrorKind::InvalidInput, format!("CDN answered {status}"))
            }
            Self::Crypto(AttachmentError::Sink(detail)) => {
                BackendError::internal(format!("writing the attachment failed: {detail}"))
            }
            Self::Crypto(e) => BackendError::new(ErrorKind::InvalidInput, e.to_string()),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use aes::Aes256Enc;
    use aes::cipher::BlockCipherEncrypt as _;

    use super::*;

    /// Encrypts like `encryptAttachmentV2` (tests only; the send path is S12): returns the blob
    /// and its digest.
    pub(crate) fn encrypt(
        key: &[u8; 64],
        iv: [u8; 16],
        plaintext: &[u8],
        pad: bool,
    ) -> (Vec<u8>, [u8; 32]) {
        let mut padded = plaintext.to_vec();
        if pad {
            padded.resize(log_pad_size(plaintext.len() as u64) as usize, 0);
        }
        let pkcs = BLOCK_LEN - padded.len() % BLOCK_LEN;
        padded.extend(std::iter::repeat_n(u8::try_from(pkcs).unwrap(), pkcs));
        let cipher = Aes256Enc::new((&<[u8; 32]>::try_from(&key[..32]).unwrap()).into());
        let mut blob = iv.to_vec();
        let mut previous = iv;
        for chunk in padded.chunks(BLOCK_LEN) {
            let mut block = [0u8; BLOCK_LEN];
            for (b, (p, c)) in block.iter_mut().zip(chunk.iter().zip(previous)) {
                *b = p ^ c;
            }
            cipher.encrypt_block((&mut block).into());
            blob.extend_from_slice(&block);
            previous = block;
        }
        let mut mac = Hmac::<Sha256>::new_from_slice(&key[32..]).unwrap();
        mac.update(&blob);
        blob.extend_from_slice(&mac.finalize().into_bytes());
        let digest = Sha256::digest(&blob).into();
        (blob, digest)
    }

    fn keys(key: &[u8; 64]) -> AttachmentKeys {
        AttachmentKeys::from_slice(key).unwrap()
    }

    /// libsignal-service-rs's known answer (`attachment_cipher.rs` `know_answer_test_attachment`):
    /// a blob Signal encrypted, with its key; the pointer's size is 32.
    #[test]
    fn libsignal_service_known_answer_decrypts() {
        let blob: Vec<u8> = include!("../testdata/lss_attachment_kat.in");
        let key: [u8; 64] = [
            52, 102, 97, 87, 153, 192, 64, 116, 93, 96, 57, 110, 6, 197, 208, 85, 49, 249, 154,
            137, 116, 124, 112, 107, 8, 158, 48, 4, 8, 66, 173, 5, 28, 16, 199, 226, 234, 38, 69,
            167, 163, 34, 107, 164, 15, 118, 101, 146, 34, 213, 85, 164, 110, 83, 129, 245, 62, 44,
            158, 78, 205, 62, 153, 108,
        ];
        let digest: [u8; 32] = Sha256::digest(&blob).into();
        let plain =
            decrypt_in_memory(&keys(&key), Integrity::Digest(digest), 32, &blob, 1 << 20).unwrap();
        assert_eq!(&plain[..], b"test for libsignal-service-rust\n");
        // The same blob under a wrong digest, a wrong key, or with a flipped bit is refused.
        let mut wrong = digest;
        wrong[0] ^= 1;
        assert_eq!(
            decrypt_in_memory(&keys(&key), Integrity::Digest(wrong), 32, &blob, 1 << 20)
                .unwrap_err(),
            AttachmentError::BadDigest
        );
        let mut other_key = key;
        other_key[40] ^= 1;
        assert_eq!(
            decrypt_in_memory(
                &keys(&other_key),
                Integrity::Digest(digest),
                32,
                &blob,
                1 << 20
            )
            .unwrap_err(),
            AttachmentError::BadMac
        );
        let mut tampered = blob.clone();
        tampered[20] ^= 0x80;
        assert_eq!(
            decrypt_in_memory(
                &keys(&key),
                Integrity::Digest(digest),
                32,
                &tampered,
                1 << 20
            )
            .unwrap_err(),
            AttachmentError::BadMac
        );
        assert_eq!(
            decrypt_in_memory(&keys(&key), Integrity::Digest(digest), 32, &blob, 64).unwrap_err(),
            AttachmentError::TooLarge
        );
    }

    /// Desktop's padding buckets (`SD/ts/test-node/util/logPadding_test.node.ts`): every size up
    /// to a bucket pads to it, one more byte to the next.
    #[test]
    fn padding_follows_desktop_buckets() {
        const BUCKETS: [usize; 10] = [
            541,
            568,
            1020,
            1071,
            10_110,
            10_616,
            100_155,
            1_041_743,
            10_319_484,
            102_224_512,
        ];
        const NEXT: [usize; 10] = [
            568,
            596,
            1071,
            1125,
            10_616,
            11_146,
            105_163,
            1_093_831,
            10_835_458,
            107_335_738,
        ];
        assert_eq!(log_pad_size(0), 541);
        assert_eq!(log_pad_size(1), 541);
        for (bucket, next) in BUCKETS.into_iter().zip(NEXT) {
            let (bucket, next) = (bucket as u64, next as u64);
            assert_eq!(log_pad_size(bucket), bucket);
            assert_eq!(log_pad_size(bucket - 1), bucket);
            assert_eq!(log_pad_size(bucket + 1), next);
        }
        // The blob of the largest padded size still fits the download bound.
        for size in [0u64, 1, 15, 16, 541, 1000, 70_000] {
            let key = [1u8; 64];
            let (blob, _) = encrypt(&key, [0; 16], &vec![0; size as usize], true);
            assert!(blob.len() as u64 <= max_blob_len(size), "{size}");
        }
    }

    #[test]
    fn padded_blobs_stream_in_any_chunks_and_strip_to_size() {
        let key: [u8; 64] = std::array::from_fn(|i| u8::try_from(i).unwrap());
        for len in [0usize, 1, 15, 16, 17, 541, 1000, 70_000] {
            let plaintext: Vec<u8> = (0..len).map(|i| (i * 7 % 251) as u8).collect();
            for pad in [false, true] {
                let (blob, digest) = encrypt(&key, [9; 16], &plaintext, pad);
                for chunk in [1usize, 7, 16, 33, 4096, blob.len()] {
                    let mut decryptor = Decryptor::new(
                        &keys(&key),
                        Integrity::Digest(digest),
                        len as u64,
                        blob.len() as u64,
                    )
                    .unwrap();
                    let mut out = Vec::new();
                    let mut sink = |p: &[u8]| {
                        out.extend_from_slice(p);
                        Ok(())
                    };
                    for piece in blob.chunks(chunk) {
                        decryptor.update(piece, &mut sink).unwrap();
                    }
                    let verified = decryptor.finish(&mut sink).unwrap();
                    assert_eq!(out, plaintext, "len {len} pad {pad} chunk {chunk}");
                    assert_eq!(verified.size, len as u64);
                    assert_eq!(
                        verified.plaintext_hash,
                        <[u8; 32]>::from(Sha256::digest(&plaintext))
                    );
                }
                // The plaintext hash is an alternative to the digest.
                let hash: [u8; 32] = Sha256::digest(&plaintext).into();
                assert!(
                    decrypt_in_memory(
                        &keys(&key),
                        Integrity::PlaintextHash(hash),
                        len as u64,
                        &blob,
                        u64::MAX
                    )
                    .is_ok()
                );
            }
        }
    }

    #[test]
    fn short_truncated_and_misstated_blobs_are_refused() {
        let key = [3u8; 64];
        let (blob, digest) = encrypt(&key, [1; 16], b"hello world", true);
        let decrypt = |blob: &[u8], size: u64| {
            decrypt_in_memory(&keys(&key), Integrity::Digest(digest), size, blob, u64::MAX)
        };
        assert_eq!(
            decrypt(&blob[..40], 11).unwrap_err(),
            AttachmentError::BadLength
        );
        assert_eq!(
            decrypt(&blob[..blob.len() - 1], 11).unwrap_err(),
            AttachmentError::BadLength
        );
        assert_eq!(
            decrypt(&blob[..blob.len() - 16], 11).unwrap_err(),
            AttachmentError::BadMac
        );
        // A pointer claiming more plaintext than the blob holds.
        assert_eq!(
            decrypt(&blob, 100_000).unwrap_err(),
            AttachmentError::TooShort
        );
        assert_eq!(
            decrypt_in_memory(
                &keys(&key),
                Integrity::PlaintextHash([0; 32]),
                11,
                &blob,
                u64::MAX
            )
            .unwrap_err(),
            AttachmentError::BadPlaintextHash
        );
        assert_eq!(
            &decrypt(&blob, 5).unwrap()[..],
            b"hello",
            "size cuts the padding"
        );
        assert_eq!(
            AttachmentKeys::from_slice(&[0; 63]).unwrap_err(),
            AttachmentError::BadKey
        );
    }

    /// The streaming path writes into a `kourier_blob` file that appears only once the blob
    /// verified (a failed MAC leaves no file).
    #[test]
    fn streams_into_a_blob_file_only_when_verified() {
        let dir = tempfile::tempdir().unwrap();
        let key = [5u8; 64];
        let plaintext: Vec<u8> = (0..200_000u32).map(|i| (i % 256) as u8).collect();
        let (blob, digest) = encrypt(&key, [2; 16], &plaintext, true);
        let file_key = kourier_blob::FileKey::generate();
        let write = |blob: &[u8], name: &str| -> Result<std::path::PathBuf, AttachmentError> {
            use std::io::Write as _;
            let path = dir.path().join(name);
            let mut writer = kourier_blob::BlobWriter::create(&path, &file_key)
                .map_err(|e| AttachmentError::Sink(e.to_string()))?;
            let mut decryptor = Decryptor::new(
                &keys(&key),
                Integrity::Digest(digest),
                plaintext.len() as u64,
                u64::MAX,
            )?;
            let mut sink = |p: &[u8]| {
                writer
                    .write_all(p)
                    .map_err(|e| AttachmentError::Sink(e.to_string()))
            };
            for piece in blob.chunks(65_536) {
                decryptor.update(piece, &mut sink)?;
            }
            decryptor.finish(&mut sink)?;
            writer
                .finish()
                .map_err(|e| AttachmentError::Sink(e.to_string()))?;
            Ok(path)
        };
        let path = write(&blob, "good").unwrap();
        assert_eq!(
            &kourier_blob::read_all(&path, &file_key).unwrap()[..],
            &plaintext[..]
        );
        let mut tampered = blob.clone();
        let at = tampered.len() - 40;
        tampered[at] ^= 1;
        assert_eq!(
            write(&tampered, "bad").unwrap_err(),
            AttachmentError::BadMac
        );
        assert!(!dir.path().join("bad").exists());
        assert!(
            !dir.path().join("bad.tmp").exists(),
            "the unverified file is removed"
        );
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// Encrypts `plaintext` fed in `chunk`-sized pieces.
    fn encrypt_with(
        key: &[u8; 64],
        iv: [u8; 16],
        plaintext: &[u8],
        incremental: bool,
        chunk: usize,
    ) -> (Vec<u8>, Encrypted) {
        let mut encryptor =
            Encryptor::new(&keys(key), iv, plaintext.len() as u64, incremental).unwrap();
        let mut blob = Vec::new();
        for piece in plaintext.chunks(chunk.max(1)) {
            encryptor.update(piece, &mut blob).unwrap();
        }
        let encrypted = encryptor.finish(&mut blob).unwrap();
        (blob, encrypted)
    }

    /// Known answers from independent implementations: the blob of Node's OpenSSL with
    /// Desktop's padding (the pipeline of `encryptAttachmentV2`) and the incremental MAC of
    /// `@signalapp/libsignal-client` 0.100 (`DigestingPassThrough` with `inferChunkSize`).
    #[test]
    fn encryption_matches_independent_known_answers() {
        let key: [u8; 64] = std::array::from_fn(|i| u8::try_from((i * 3 + 1) & 0xff).unwrap());
        let iv: [u8; 16] = std::array::from_fn(|i| 0xa0 + u8::try_from(i).unwrap());
        let text = b"Kourier attachment vector: AES-256-CBC + HMAC-SHA256, Desktop padding.\n";
        let (blob, encrypted) = encrypt_with(&key, iv, text, false, 7);
        assert_eq!(blob.len(), 592);
        assert_eq!(encrypted.len, 592);
        assert_eq!(&blob[..16], &iv);
        assert_eq!(
            hex(&blob[16..48]),
            "5cd473f5dea98a9221c8c3196d6c6e20619ae95bf88eaa83628b5f6cad3d336b"
        );
        assert_eq!(
            hex(&blob[blob.len() - 32..]),
            "64e777d07a1d91cdda0067bbc3f6e9e4294cc842e902f42e5a26ff70fb24ae48"
        );
        assert_eq!(
            hex(&encrypted.digest),
            "c650ed1543615f37b4cb14ca0589f19542ac6dc5300e0f58fba24fabd5c85a25"
        );
        assert_eq!(
            encrypted.plaintext_hash,
            <[u8; 32]>::from(Sha256::digest(text))
        );
        assert_eq!(
            (encrypted.incremental_mac, encrypted.chunk_size),
            (None, None)
        );

        let pattern = |len: usize| -> Vec<u8> {
            (0..len)
                .map(|i| u8::try_from(i * 7 % 251).unwrap())
                .collect()
        };
        for (len, blob_len, digest, macs, macs_sha) in [
            (
                200_000,
                208_272,
                "194b47ca50eca9ac3c9d54916f5032a0767b7d836e6b5685b11dd8bbb2ba276d",
                128,
                "72a35b3c28b6b40d84d4091a57cb4887e56308862538bb7a6f63ab7b41a63ebb",
            ),
            (
                5,
                592,
                "0270998baae27ca552fbbc6cd35365aeeaafaaea9314df5e30abff92dc4d66da",
                32,
                "0065fafe043547d2fce86e2e2d9a92ff2978d639a9324ce52429e0687298949f",
            ),
        ] {
            let (blob, encrypted) = encrypt_with(&key, iv, &pattern(len), true, 65_536);
            assert_eq!(blob.len(), blob_len, "{len}");
            assert_eq!(max_blob_len(len as u64), blob_len as u64);
            assert_eq!(hex(&encrypted.digest), digest, "{len}");
            assert_eq!(encrypted.chunk_size, Some(65_536));
            let incremental = encrypted.incremental_mac.unwrap();
            assert_eq!(incremental.len(), macs, "{len}");
            assert_eq!(hex(&Sha256::digest(&incremental)), macs_sha, "{len}");
        }
    }

    /// The blob does not depend on how the plaintext is fed, decrypts back, and a source that
    /// gives more or less than it declared is refused.
    #[test]
    fn encrypted_blobs_decrypt_and_sizes_are_enforced() {
        let key: [u8; 64] = std::array::from_fn(|i| u8::try_from(255 - i).unwrap());
        for len in [0usize, 1, 15, 16, 17, 541, 70_000] {
            let plaintext: Vec<u8> = (0..len).map(|i| (i * 13 % 256) as u8).collect();
            let (whole, encrypted) = encrypt_with(&key, [4; 16], &plaintext, true, len.max(1));
            for chunk in [1usize, 15, 16, 33, 4096] {
                let (blob, again) = encrypt_with(&key, [4; 16], &plaintext, true, chunk);
                assert_eq!(blob, whole, "len {len} chunk {chunk}");
                assert!(again == encrypted);
            }
            assert_eq!(whole.len() as u64, max_blob_len(len as u64));
            let plain = decrypt_in_memory(
                &keys(&key),
                Integrity::Digest(encrypted.digest),
                len as u64,
                &whole,
                u64::MAX,
            )
            .unwrap();
            assert_eq!(&plain[..], &plaintext[..], "len {len}");
            // The test helper (the decryptor tests' encryption) agrees byte for byte.
            assert_eq!(encrypt(&key, [4; 16], &plaintext, true).0, whole);
        }
        let mut encryptor = Encryptor::new(&keys(&key), [0; 16], 3, false).unwrap();
        let mut out = Vec::new();
        assert_eq!(
            encryptor.update(b"four", &mut out).unwrap_err(),
            AttachmentError::TooLarge
        );
        encryptor.update(b"ab", &mut out).unwrap();
        assert_eq!(
            encryptor.finish(&mut out).unwrap_err(),
            AttachmentError::TooShort
        );
        assert!(supports_incremental_mac("video/MP4"));
        assert!(!supports_incremental_mac("video/webm"));
    }

    #[test]
    fn pointers_need_key_digest_size_and_a_safe_cdn_key() {
        let pointer = proto::AttachmentPointer {
            attachment_identifier: Some(proto::attachment_pointer::AttachmentIdentifier::CdnKey(
                "abc_DEF-123".to_owned(),
            )),
            key: Some(vec![1; 64]),
            digest: Some(vec![2; 32]),
            size: Some(10),
            cdn_number: Some(3),
            ..Default::default()
        };
        let parsed = Pointer::from_proto(&pointer).unwrap();
        assert_eq!(
            (parsed.cdn, parsed.cdn_key.as_str(), parsed.size),
            (3, "abc_DEF-123", 10)
        );
        let request = parsed.request(std::time::Duration::from_secs(5));
        assert_eq!(request.host, HttpsHost::Cdn(3));
        assert_eq!(request.path, "/attachments/abc_DEF-123");
        assert!(
            !format!("{parsed:?}").contains("abc"),
            "Debug hides the CDN key"
        );

        let legacy = proto::AttachmentPointer {
            attachment_identifier: Some(proto::attachment_pointer::AttachmentIdentifier::CdnId(42)),
            cdn_number: None,
            ..pointer.clone()
        };
        let parsed = Pointer::from_proto(&legacy).unwrap();
        assert_eq!((parsed.cdn, parsed.cdn_key.as_str()), (0, "42"));
        for bad in [
            proto::AttachmentPointer {
                attachment_identifier: Some(
                    proto::attachment_pointer::AttachmentIdentifier::CdnKey("../x".to_owned()),
                ),
                ..pointer.clone()
            },
            proto::AttachmentPointer {
                key: Some(vec![1; 32]),
                ..pointer.clone()
            },
            proto::AttachmentPointer {
                digest: None,
                ..pointer.clone()
            },
            proto::AttachmentPointer {
                size: None,
                ..pointer.clone()
            },
        ] {
            assert!(Pointer::from_proto(&bad).is_err());
        }
    }

    /// A scripted answer to a whole-body request: a status and a body, or a failure below HTTP.
    pub(crate) type Answer = Result<(u16, Vec<u8>), crate::net::TransportError>;

    /// A CDN that answers every streamed download with a status and its blob (`pieces`, from
    /// the position the request asks for, as a CDN answers a range request) and, when set, a
    /// failure of the connection after it; and the whole-body requests with its scripted
    /// answers, one after the other. `cuts` makes transfers break off earlier: each streamed
    /// request takes the next entry and fails with `cut_error` once the blob position reaches
    /// it; with none left the request runs to the end of the blob.
    pub(crate) struct ScriptedCdn {
        status: u16,
        pieces: Vec<Vec<u8>>,
        then: Option<crate::net::TransportError>,
        answers: std::sync::Mutex<std::collections::VecDeque<Answer>>,
        /// Blob positions at which the next streamed requests break off.
        cuts: std::sync::Mutex<std::collections::VecDeque<usize>>,
        /// A byte position to corrupt in what is served after the first request (a CDN that
        /// serves another blob to the resumed request).
        corrupt_resumed_at: Option<usize>,
        /// Host and path of every request.
        pub(crate) requests: std::sync::Mutex<Vec<(HttpsHost, String)>>,
        /// The blob position each streamed request started at.
        pub(crate) offsets: std::sync::Mutex<Vec<u64>>,
    }

    /// How a cut transfer fails.
    fn cut_error() -> crate::net::TransportError {
        crate::net::TransportError::Https("connection reset".to_owned())
    }

    impl ScriptedCdn {
        fn new(
            status: u16,
            pieces: Vec<Vec<u8>>,
            then: Option<crate::net::TransportError>,
        ) -> std::sync::Arc<Self> {
            Self::cutting(status, pieces, then, Vec::new(), None)
        }

        /// A CDN whose streamed transfers break off at `cuts`, one per request.
        fn cutting(
            status: u16,
            pieces: Vec<Vec<u8>>,
            then: Option<crate::net::TransportError>,
            cuts: Vec<usize>,
            corrupt_resumed_at: Option<usize>,
        ) -> std::sync::Arc<Self> {
            std::sync::Arc::new(Self {
                status,
                pieces,
                then,
                answers: std::sync::Mutex::default(),
                cuts: std::sync::Mutex::new(cuts.into()),
                corrupt_resumed_at,
                requests: std::sync::Mutex::new(Vec::new()),
                offsets: std::sync::Mutex::new(Vec::new()),
            })
        }

        /// A CDN whose whole-body requests get `answers` in turn.
        pub(crate) fn answering(answers: Vec<Answer>) -> std::sync::Arc<Self> {
            std::sync::Arc::new(Self {
                status: 404,
                pieces: Vec::new(),
                then: None,
                answers: std::sync::Mutex::new(answers.into()),
                cuts: std::sync::Mutex::default(),
                corrupt_resumed_at: None,
                requests: std::sync::Mutex::new(Vec::new()),
                offsets: std::sync::Mutex::new(Vec::new()),
            })
        }
    }

    impl SignalTransport for ScriptedCdn {
        type Socket = crate::net::libsignal::LibsignalSocket;

        async fn connect_auth(
            &self,
            _credentials: &crate::net::AuthCredentials,
        ) -> Result<crate::net::Connected<Self::Socket, crate::net::ChatEvent>, TransportError>
        {
            Err(TransportError::Disconnected)
        }

        async fn connect_unauth(
            &self,
        ) -> Result<crate::net::Connected<Self::Socket, crate::net::ChatEvent>, TransportError>
        {
            Err(TransportError::Disconnected)
        }

        async fn connect_provisioning(
            &self,
        ) -> Result<
            crate::net::Connected<Self::Socket, crate::net::ProvisioningEvent>,
            TransportError,
        > {
            Err(TransportError::Disconnected)
        }

        async fn https(
            &self,
            request: HttpsRequest,
        ) -> Result<crate::net::HttpsResponse, TransportError> {
            self.requests
                .lock()
                .unwrap()
                .push((request.host, request.path.clone()));
            let answer = self.answers.lock().unwrap().pop_front();
            let (status, body) = answer.unwrap_or(Err(TransportError::Disconnected))?;
            Ok(crate::net::HttpsResponse {
                status: http::StatusCode::from_u16(status).unwrap(),
                headers: http::HeaderMap::new(),
                body: body.into(),
            })
        }

        async fn https_download(
            &self,
            request: HttpsRequest,
            sink: crate::net::DownloadSink,
        ) -> Result<http::StatusCode, TransportError> {
            self.requests
                .lock()
                .unwrap()
                .push((request.host, request.path.clone()));
            // The environment has CDN 0, 2 and 3, as `HttpsClient::url` knows them.
            if !matches!(request.host, HttpsHost::Cdn(0 | 2 | 3)) {
                return Err(TransportError::InvalidRequest("unknown CDN"));
            }
            let status = http::StatusCode::from_u16(self.status).unwrap();
            if !status.is_success() {
                return Ok(status);
            }
            // The rest of the blob from where the request starts, as a range request gets it.
            let start = usize::try_from(sink.offset()).unwrap();
            let first = self.offsets.lock().unwrap().is_empty();
            self.offsets.lock().unwrap().push(sink.offset());
            let mut blob: Vec<u8> = self.pieces.concat();
            if start > 0 && start >= blob.len() && self.then.is_none() {
                // No byte from there on (RFC 9110 §15.5.17). A CDN scripted to fail after
                // its pieces (`then`) models a connection that keeps breaking instead.
                return Ok(http::StatusCode::RANGE_NOT_SATISFIABLE);
            }
            if let Some(at) = self.corrupt_resumed_at.filter(|_| !first) {
                blob[at] ^= 0x55;
            }
            let cut = self.cuts.lock().unwrap().pop_front();
            let end = cut.map_or(blob.len(), |cut| cut.min(blob.len()));
            for piece in blob[start.min(end)..end].chunks(8192) {
                sink.send(bytes::Bytes::copy_from_slice(piece))
                    .await
                    .map_err(|_| {
                        TransportError::InvalidResponse(
                            "the download's consumer stopped".to_owned(),
                        )
                    })?;
            }
            if cut.is_some() {
                return Err(cut_error());
            }
            match &self.then {
                Some(error) => Err(error.clone()),
                None => Ok(status),
            }
        }

        async fn https_upload(
            &self,
            _request: crate::net::UploadRequest,
        ) -> Result<crate::net::HttpsResponse, TransportError> {
            Err(TransportError::Disconnected)
        }

        fn network_changed(&self) {}
    }

    use crate::net::TransportError;

    /// The production policy without its waits.
    const QUICK: ResumePolicy = ResumePolicy {
        fruitless: ResumePolicy::DEFAULT.fruitless,
        pause: std::time::Duration::ZERO,
    };

    /// A transfer that breaks off goes on from the byte it stopped at, as often as it makes
    /// progress, and the blob is verified as a whole at the end: what arrives is the file,
    /// byte for byte, and a resumed part that is not the rest of the same blob is refused.
    /// Requests that bring nothing are counted, and after five in a row the download fails
    /// as the network failure it is.
    #[tokio::test]
    async fn a_download_that_breaks_off_goes_on_where_it_stopped() {
        let key = [6u8; 64];
        let plaintext: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        let (blob, digest) = encrypt(&key, [3; 16], &plaintext, true);
        let size = plaintext.len() as u64;
        let pointer = Pointer {
            cdn: 3,
            cdn_key: "abc".to_owned(),
            key: Zeroizing::new(key.to_vec()),
            digest,
            size,
            content_type: Some("video/mp4".to_owned()),
            dims: None,
        };
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("blobs").join("download");
        let file_key = kourier_model::BlobKey::new([9; 32]);
        let run = async |cdn: &std::sync::Arc<ScriptedCdn>| {
            let net = Net::new(cdn.clone(), tokio::runtime::Handle::current());
            let written = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
            let result = download_to_blob(
                &net,
                &pointer,
                &dest,
                &file_key,
                1 << 20,
                std::time::Duration::from_secs(5),
                QUICK,
                &written,
            )
            .await;
            (result, written.load(std::sync::atomic::Ordering::Relaxed))
        };
        let offsets = |cdn: &ScriptedCdn| cdn.offsets.lock().unwrap().clone();

        // Three breaks (one of them on a cipher block boundary, one in the middle of a
        // block, one right after the other without a byte in between), then the rest.
        let cuts = vec![16 + 16 * 1000, 100_003, 100_003, 250_000];
        let cdn = ScriptedCdn::cutting(200, vec![blob.clone()], None, cuts, None);
        let (result, written) = run(&cdn).await;
        let meta = result.expect("resumed to the end");
        assert_eq!(meta.size, size);
        assert_eq!(written, size, "every plaintext byte was counted once");
        assert_eq!(
            offsets(&cdn),
            [0, 16_016, 100_003, 100_003, 250_000],
            "each request asks for the rest from the byte the last one stopped at"
        );
        let blob_key = kourier_blob::FileKey::from_bytes(*file_key.expose());
        assert_eq!(
            &kourier_blob::read_all(&dest, &blob_key).unwrap()[..],
            &plaintext[..],
            "the file, byte for byte"
        );
        std::fs::remove_file(&dest).unwrap();

        // The resumed request gets other bytes than the rest of the blob it started: the
        // MAC over the whole stream refuses it, and nothing is left behind.
        let cdn = ScriptedCdn::cutting(200, vec![blob.clone()], None, vec![50_000], Some(60_000));
        let (result, _) = run(&cdn).await;
        let error = result.unwrap_err();
        assert!(
            matches!(
                error,
                DownloadError::Crypto(AttachmentError::BadMac | AttachmentError::BadDigest)
            ),
            "{error:?}"
        );
        assert_eq!(
            error.to_backend_error().kind,
            kourier_model::ErrorKind::InvalidInput
        );
        assert!(!dest.exists());

        // The connection stays dead after the first part: five requests in a row bring
        // nothing, then the download fails as a network failure, to be tried again later.
        let cuts = vec![80_000; 20];
        let cdn = ScriptedCdn::cutting(200, vec![blob.clone()], None, cuts, None);
        let (result, written) = run(&cdn).await;
        let error = result.unwrap_err();
        assert!(matches!(error, DownloadError::Transport(_)), "{error:?}");
        assert!(error.to_backend_error().is_retryable());
        assert_eq!(offsets(&cdn), [0, 80_000, 80_000, 80_000, 80_000, 80_000]);
        assert!(written > 0 && written < size);
        assert!(!dest.exists(), "nothing unverified is left");

        // Fruitless requests count in a row only: progress in between starts the count anew.
        let cuts = vec![
            10_000, 10_000, 10_000, 10_000, 20_000, 20_000, 20_000, 20_000,
        ];
        let cdn = ScriptedCdn::cutting(200, vec![blob.clone()], None, cuts, None);
        let (result, _) = run(&cdn).await;
        result.expect("eight breaks, never five without a byte");
        assert_eq!(offsets(&cdn).len(), 9);

        // The connection breaks after the last byte of the blob, before its clean end: the
        // CDN has nothing more (416), and what arrived is the whole file.
        let cdn = ScriptedCdn::cutting(200, vec![blob.clone()], None, vec![blob.len()], None);
        let (result, written) = run(&cdn).await;
        let meta = result.expect("every byte arrived");
        assert_eq!((meta.size, written), (size, size));
        assert_eq!(offsets(&cdn), [0, blob.len() as u64]);
        assert_eq!(
            &kourier_blob::read_all(&dest, &blob_key).unwrap()[..],
            &plaintext[..]
        );
        std::fs::remove_file(&dest).unwrap();
        // The same answer for a blob that is shorter than its pointer says is no success:
        // the verification of what arrived decides.
        let short = blob[..blob.len() - 64].to_vec();
        let cdn = ScriptedCdn::cutting(200, vec![short.clone()], None, vec![short.len()], None);
        let (result, _) = run(&cdn).await;
        assert!(
            matches!(result, Err(DownloadError::Crypto(_))),
            "{result:?}"
        );
        assert!(!dest.exists());

        // A transfer that never starts has nothing to resume: one request, as before.
        let cdn = ScriptedCdn::cutting(200, vec![blob.clone()], None, vec![0, 0, 0], None);
        let (result, _) = run(&cdn).await;
        assert!(matches!(result, Err(DownloadError::Transport(_))));
        assert_eq!(offsets(&cdn), [0]);
        // What cannot be cured by asking again is not asked again.
        for fatal in [
            TransportError::BodyTooLarge,
            TransportError::InvalidRequest("x"),
            TransportError::InvalidResponse("x".to_owned()),
        ] {
            assert!(!resumable(&fatal), "{fatal:?}");
        }
        for transient in [
            TransportError::Timeout,
            TransportError::Disconnected,
            TransportError::ServerSide,
            TransportError::Https("reset".to_owned()),
        ] {
            assert!(resumable(&transient), "{transient:?}");
        }
    }

    /// Whatever a pointer claims and whatever the CDN sends, a failed download ends with the
    /// right kind of error and leaves nothing behind: no blob file and no temporary one.
    #[tokio::test]
    async fn hostile_pointers_and_broken_transfers_fail_cleanly() {
        use kourier_model::ErrorKind;
        let key = [6u8; 64];
        let plaintext: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
        let (blob, digest) = encrypt(&key, [3; 16], &plaintext, true);
        let pointer = |size: u64, digest: [u8; 32], cdn: u32| Pointer {
            cdn,
            cdn_key: "abc".to_owned(),
            key: Zeroizing::new(key.to_vec()),
            digest,
            size,
            content_type: Some("image/png".to_owned()),
            dims: None,
        };
        let pieces = |blob: &[u8]| blob.chunks(8192).map(<[u8]>::to_vec).collect::<Vec<_>>();
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("blobs").join("download");
        let file_key = kourier_model::BlobKey::new([9; 32]);
        let limit = 1 << 20;
        let size = plaintext.len() as u64;
        let run = async |cdn: &std::sync::Arc<ScriptedCdn>, pointer: Pointer| {
            let net = Net::new(cdn.clone(), tokio::runtime::Handle::current());
            let result = download_to_blob(
                &net,
                &pointer,
                &dest,
                &file_key,
                limit,
                std::time::Duration::from_secs(5),
                QUICK,
                &std::sync::Arc::default(),
            )
            .await;
            if result.is_err() {
                let left: Vec<_> = std::fs::read_dir(dest.parent().unwrap())
                    .map(|d| d.flatten().map(|e| e.file_name()).collect())
                    .unwrap_or_default();
                assert!(left.is_empty(), "a failed download left {left:?}");
            }
            result
        };

        // A wrong digest: refused for good.
        let mut wrong = digest;
        wrong[31] ^= 1;
        let cdn = ScriptedCdn::new(200, pieces(&blob), None);
        let error = run(&cdn, pointer(size, wrong, 3)).await.unwrap_err();
        assert!(matches!(
            error,
            DownloadError::Crypto(AttachmentError::BadDigest)
        ));
        assert_eq!(error.to_backend_error().kind, ErrorKind::InvalidInput);

        // The connection drops on a block boundary: the cut stream ends in what looks like a
        // bad MAC, but the failure is the network's and worth another try.
        let cut = 16 + 16 * 1000;
        let reset = TransportError::Https("connection reset".to_owned());
        let cdn = ScriptedCdn::new(200, pieces(&blob[..cut]), Some(reset.clone()));
        let error = run(&cdn, pointer(size, digest, 3)).await.unwrap_err();
        assert!(
            matches!(&error, DownloadError::Transport(e) if *e == reset),
            "{error:?}"
        );
        assert!(error.to_backend_error().is_retryable());
        // ... and anywhere else.
        let cdn = ScriptedCdn::new(200, pieces(&blob[..cut + 5]), Some(reset.clone()));
        let error = run(&cdn, pointer(size, digest, 3)).await.unwrap_err();
        assert!(matches!(error, DownloadError::Transport(_)), "{error:?}");

        // A blob longer than the pointer's size allows (a small `size` in front of a large
        // blob): stopped by the decryptor, or by the HTTPS client's own limit.
        let cdn = ScriptedCdn::new(200, pieces(&blob), None);
        let error = run(&cdn, pointer(100, digest, 3)).await.unwrap_err();
        assert!(
            matches!(error, DownloadError::Crypto(AttachmentError::TooLarge)),
            "{error:?}"
        );
        assert_eq!(error.to_backend_error().kind, ErrorKind::TooLarge);
        let cdn = ScriptedCdn::new(200, Vec::new(), Some(TransportError::BodyTooLarge));
        let error = run(&cdn, pointer(100, digest, 3)).await.unwrap_err();
        assert_eq!(error.to_backend_error().kind, ErrorKind::TooLarge);

        // A size over the receive limit: refused before any request.
        let cdn = ScriptedCdn::new(200, pieces(&blob), None);
        let error = run(&cdn, pointer(u64::from(u32::MAX), digest, 3))
            .await
            .unwrap_err();
        assert_eq!(error.to_backend_error().kind, ErrorKind::TooLarge);
        assert!(cdn.requests.lock().unwrap().is_empty(), "nothing was asked");

        // A CDN number out of range: no request can be made, and no retry will change that.
        let cdn = ScriptedCdn::new(200, pieces(&blob), None);
        let error = run(&cdn, pointer(size, digest, 77)).await.unwrap_err();
        assert!(
            matches!(
                error,
                DownloadError::Transport(TransportError::InvalidRequest(_))
            ),
            "{error:?}"
        );
        assert_eq!(error.to_backend_error().kind, ErrorKind::InvalidInput);

        // Gone from the CDN.
        let cdn = ScriptedCdn::new(404, Vec::new(), None);
        let error = run(&cdn, pointer(size, digest, 3)).await.unwrap_err();
        assert!(matches!(error, DownloadError::Status(404)));
        assert_eq!(error.to_backend_error().kind, ErrorKind::Expired);

        // The honest transfer of the same blob arrives whole, at its CDN path.
        let cdn = ScriptedCdn::new(200, pieces(&blob), None);
        let meta = run(&cdn, pointer(size, digest, 3)).await.unwrap();
        assert_eq!(meta.size, size);
        assert_eq!(
            cdn.requests.lock().unwrap().as_slice(),
            [(HttpsHost::Cdn(3), "/attachments/abc".to_owned())]
        );
        let key = kourier_blob::FileKey::from_bytes(*file_key.expose());
        assert_eq!(
            &kourier_blob::read_all(&dest, &key).unwrap()[..],
            &plaintext[..]
        );
    }

    #[test]
    fn transfer_timeouts_grow_with_the_blob() {
        let minutes = |len: u64| transfer_timeout(len).as_secs() / 60;
        assert_eq!(minutes(0), 10);
        assert_eq!(minutes(5 * 1024 * 1024), 10);
        assert_eq!(minutes(100 * 1024 * 1024), 106);
    }
}
