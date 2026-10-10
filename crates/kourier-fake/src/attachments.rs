//! Synthetic attachments (§10.2): the fake's network pointers and its "CDN". A pointer names
//! what the download produces (a deterministic picture from [`kourier_media::sample_image`], or
//! deterministic bytes) or how it fails; `DownloadAttachment` streams the plaintext into the
//! core's `BlobTarget` like a real backend, and outgoing attachments are read back from the
//! core's blobs before a transmit is accepted.

use std::path::Path;

use kourier_backend_api::BlobTarget;
use kourier_blob::FileKey;
use kourier_media::SampleFormat;
use kourier_model::{
    Attachment, AttachmentKind, AttachmentMeta, BackendBlob, BackendError, BlobRef, ErrorKind,
};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

/// `BackendBlob::v` of the fake's pointers.
pub const POINTER_VERSION: u16 = 0x46;

/// What a fake pointer downloads to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Pointer {
    /// A JPEG picture.
    Image {
        /// Width.
        width: u32,
        /// Height.
        height: u32,
        /// Picture seed.
        seed: u64,
    },
    /// `len` pseudo-random bytes.
    Bytes {
        /// Length.
        len: u64,
        /// Content seed.
        seed: u64,
    },
    /// A text file with exactly this content (the whole text of a long message).
    Text {
        /// The content.
        text: String,
    },
    /// The CDN no longer has it (`AttachmentDone` with `ErrorKind::Expired`).
    Expired,
}

impl Pointer {
    /// As an attachment's `remote`.
    pub fn to_blob(&self) -> BackendBlob {
        BackendBlob::new(
            POINTER_VERSION,
            serde_json::to_vec(self).unwrap_or_default(),
        )
    }

    /// Parses a fake pointer.
    pub fn from_blob(b: &BackendBlob) -> Option<Self> {
        (b.v == POINTER_VERSION)
            .then(|| serde_json::from_slice(&b.bytes).ok())
            .flatten()
    }

    /// The plaintext the download writes (`None` for a failing pointer).
    pub fn plaintext(&self) -> Option<Result<Vec<u8>, BackendError>> {
        match self {
            Self::Image {
                width,
                height,
                seed,
            } => Some(
                kourier_media::sample_image(*width, *height, *seed, SampleFormat::Jpeg)
                    .map_err(|e| BackendError::internal(format!("fake image: {e}"))),
            ),
            Self::Bytes { len, seed } => Some(Ok(bytes(*len, *seed))),
            Self::Text { text } => Some(Ok(text.clone().into_bytes())),
            Self::Expired => None,
        }
    }
}

fn bytes(len: u64, seed: u64) -> Vec<u8> {
    let mut x = seed ^ 0x2545_f491_4f6c_dd1d;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            u8::try_from(x & 0xff).unwrap_or(0)
        })
        .collect()
}

/// An incoming picture attachment whose download yields a `width` × `height` JPEG (the size is
/// an estimate; `AttachmentDone` reports the real one).
pub fn synthetic_image(width: u32, height: u32, seed: u64) -> Attachment {
    Attachment {
        kind: AttachmentKind::Image,
        mime: "image/jpeg".into(),
        size: u64::from(width) * u64::from(height) / 8,
        dims: Some((width, height)),
        remote: Some(
            Pointer::Image {
                width,
                height,
                seed,
            }
            .to_blob(),
        ),
        ..Attachment::default()
    }
}

/// An incoming file attachment of `len` deterministic bytes.
pub fn synthetic_file(name: &str, mime: &str, len: u64, seed: u64) -> Attachment {
    Attachment {
        kind: AttachmentKind::File,
        mime: mime.into(),
        size: len,
        file_name: Some(name.into()),
        remote: Some(Pointer::Bytes { len, seed }.to_blob()),
        ..Attachment::default()
    }
}

/// The whole text of a message whose body arrives trimmed: an attachment marked
/// `Attachment::long_text` whose download yields `text` (what a backend emits when it could
/// not fetch the long text itself). The core fetches it and joins it into the body.
pub fn synthetic_long_text(text: &str) -> Attachment {
    Attachment {
        kind: AttachmentKind::File,
        mime: "text/x-fake-long-text".into(),
        size: u64::try_from(text.len()).unwrap_or(u64::MAX),
        remote: Some(Pointer::Text { text: text.into() }.to_blob()),
        long_text: true,
        ..Attachment::default()
    }
}

/// An incoming attachment the CDN no longer has.
pub fn expired_attachment(kind: AttachmentKind) -> Attachment {
    Attachment {
        kind,
        mime: "image/jpeg".into(),
        size: 1000,
        remote: Some(Pointer::Expired.to_blob()),
        ..Attachment::default()
    }
}

/// Downloads `remote` into `dest` (blocking): the plaintext goes through `kourier-blob`'s writer
/// with the core's key. Returns what `AttachmentDone` reports.
pub(crate) fn download(
    remote: &BackendBlob,
    dest: &BlobTarget,
) -> Result<AttachmentMeta, BackendError> {
    let pointer = Pointer::from_blob(remote)
        .ok_or_else(|| BackendError::unsupported("not a fake attachment pointer"))?;
    let plain = match pointer.plaintext() {
        Some(p) => Zeroizing::new(p?),
        None => {
            return Err(BackendError::new(
                ErrorKind::Expired,
                "gone from the fake CDN",
            ));
        }
    };
    let key = FileKey::from_bytes(*dest.key.expose());
    let size = kourier_blob::write_all(&dest.path, &key, &plain)
        .map_err(|e| BackendError::internal(format!("fake download: {e}")))?;
    let (mime, dims) = match pointer {
        Pointer::Image { width, height, .. } => {
            (Some("image/jpeg".to_owned()), Some((width, height)))
        }
        _ => (None, None),
    };
    Ok(AttachmentMeta {
        size,
        mime,
        dims,
        duration_ms: None,
    })
}

/// Reads an outgoing attachment's blob completely, as a real backend does before it uploads
/// (blocking). The size must match the attachment's.
pub(crate) fn read_outgoing(blob_dir: &Path, a: &Attachment) -> Result<u64, BackendError> {
    let BlobRef { path, key } = a
        .local
        .as_ref()
        .ok_or_else(|| BackendError::new(ErrorKind::InvalidInput, "attachment without a file"))?;
    let plain = kourier_blob::read_all(&blob_dir.join(path), &FileKey::from_bytes(*key.expose()))
        .map_err(|e| {
        BackendError::new(
            ErrorKind::InvalidInput,
            format!("attachment unreadable: {e}"),
        )
    })?;
    let len = u64::try_from(plain.len()).unwrap_or(u64::MAX);
    if len != a.size {
        return Err(BackendError::new(
            ErrorKind::InvalidInput,
            "attachment size does not match its file",
        ));
    }
    Ok(len)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kourier_model::BlobKey;

    #[test]
    fn pointers_round_trip_and_download_into_the_target() {
        let a = synthetic_image(64, 48, 9);
        let remote = a.remote.clone().expect("remote");
        let p = Pointer::from_blob(&remote).expect("pointer");
        assert_eq!(
            p,
            Pointer::Image {
                width: 64,
                height: 48,
                seed: 9
            }
        );
        let dir = tempfile::tempdir().expect("tempdir");
        let dest = BlobTarget {
            path: dir.path().join("blob"),
            key: BlobKey::new([3; 32]),
        };
        let meta = download(&remote, &dest).expect("download");
        let plain =
            kourier_blob::read_all(&dest.path, &FileKey::from_bytes([3; 32])).expect("read");
        assert_eq!(meta.size, plain.len() as u64);
        assert_eq!(*plain, p.plaintext().expect("bytes").expect("ok"));
        assert_eq!(meta.dims, Some((64, 48)));

        let file = synthetic_file("a.bin", "application/octet-stream", 100, 1);
        let dest2 = BlobTarget {
            path: dir.path().join("blob2"),
            key: BlobKey::new([4; 32]),
        };
        assert_eq!(
            download(file.remote.as_ref().expect("remote"), &dest2)
                .expect("file")
                .size,
            100
        );
        let gone = expired_attachment(AttachmentKind::Image);
        let err = download(gone.remote.as_ref().expect("remote"), &dest2).expect_err("expired");
        assert_eq!(err.kind, ErrorKind::Expired);
        let err = download(&BackendBlob::new(1, vec![1]), &dest2).expect_err("foreign");
        assert_eq!(err.kind, ErrorKind::Unsupported);
    }
}
