//! Attachments in the core (§5.8): the auto-download policy, download jobs with fresh blob
//! targets, thumbnails and blurhashes derived after a download, outgoing file import, plaintext
//! playback files and the startup sweep of orphaned blobs.
//!
//! State machine of an incoming attachment row (`attachments.state`):
//!
//! ```text
//! Remote ──policy or click──▶ Downloading{0,size} + attachment_downloads row (path, key)
//!   ▲                              │ resent at every actor start (≤ MAX_DOWNLOAD_RESENDS)
//!   │                              ▼
//!   │         AttachmentDone(Ok) ─▶ Local + files row (the target) ─▶ derive: thumb + blurhash
//!   └─ click ◀ AttachmentDone(Err) ─▶ Failed{retryable} | Expired (target unlinked)
//! ```
//!
//! A long text (`Attachment.long_text`: the whole text of a message whose body arrived trimmed)
//! takes the same path, fetched whatever the download settings say; its derive step reads the
//! file and joins it into the body, and the attachment row and file go ([`derive`]).
//!
//! Every arrow is one `core.db` transaction; the commands and the file work run after the
//! commit, so a kill anywhere resumes from the rows: an unanswered download is resent with the
//! same target, an underived file is derived at the next start, a replayed `AttachmentDone` is a
//! no-op.

pub(crate) mod derive;
pub(crate) mod import;
pub(crate) mod playback;
pub(crate) mod policy;
pub(crate) mod sweep;

use std::path::{Path, PathBuf};

use kourier_backend_api::{BlobTarget, Command};
use kourier_blob::{BlobId, BlobStore};
use kourier_model::{
    AttachmentId, AttachmentKind, BackendBlob, BlobKey, ConversationId, MessageId, Redacted,
};

/// The core's own subtree of an account's blob directory: attachments, download targets and
/// thumbnails. Backends keep their own blobs (avatars) beside it and manage them themselves, so
/// the core's startup sweep never touches a file a backend may still reference.
pub(crate) const CORE_BLOBS: &str = "core";

/// The core's blob store of an account (`<blob_dir>/core`, created 0700).
pub(crate) fn core_store(blob_dir: &Path) -> std::io::Result<BlobStore> {
    BlobStore::open(&blob_dir.join(CORE_BLOBS))
}

/// `core/<2 hex>/<id>`: where a core blob lives, relative to the account's blob directory (the
/// form `files.path`, download targets and `BlobRef`s use).
pub(crate) fn core_path(id: &BlobId) -> PathBuf {
    Path::new(CORE_BLOBS).join(id.relative_path())
}

/// The blob id of a [`core_path`].
pub(crate) fn core_id(path: &Path) -> Option<BlobId> {
    BlobId::from_relative(path.strip_prefix(CORE_BLOBS).ok()?)
}

/// How many actor starts may resend an unanswered download before it is marked
/// `Failed{retryable}` (the user can click again).
pub(crate) const MAX_DOWNLOAD_RESENDS: i64 = 5;

/// A download the core decided on (§4.2 `DownloadAttachment`).
#[derive(Clone)]
pub(crate) struct DownloadJob {
    /// The attachment row.
    pub attachment: AttachmentId,
    /// The request (`attachment_downloads.id`), echoed in `AttachmentDone`.
    pub request: kourier_model::DownloadId,
    /// The network pointer.
    pub remote: BackendBlob,
    /// The fresh target, relative to the account's blob directory.
    pub path: PathBuf,
    /// The target's file key.
    pub key: BlobKey,
    /// The attachment's names (`Attachment::net_ids`), handed to the backend so that it can
    /// check the file against a name that names content (a plaintext hash).
    pub net_ids: Vec<String>,
}

impl std::fmt::Debug for DownloadJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DownloadJob")
            .field("attachment", &self.attachment)
            .field("request", &self.request)
            .finish_non_exhaustive()
    }
}

impl DownloadJob {
    /// The backend command, with the target made absolute.
    pub(crate) fn command(&self, blob_dir: &Path) -> Command {
        Command::DownloadAttachment {
            attachment: self.attachment,
            request: self.request,
            remote: self.remote.clone(),
            dest: BlobTarget {
                path: blob_dir.join(&self.path),
                key: self.key.clone(),
            },
            net_ids: Redacted::new(self.net_ids.clone()),
        }
    }
}

/// A local file to derive a thumbnail, blurhash and dimensions from (after a download, or at the
/// next start when a kill came first).
#[derive(Clone)]
pub(crate) struct DeriveJob {
    /// The attachment row.
    pub attachment: AttachmentId,
    /// Its message.
    pub message: MessageId,
    /// Its conversation.
    pub conv: ConversationId,
    /// The `files` row the job reads; the result applies only while it is still the file.
    pub file_id: i64,
    /// The file, relative to the account's blob directory.
    pub path: PathBuf,
    /// Its key.
    pub key: BlobKey,
    /// MIME type, when known.
    pub mime: Option<String>,
    /// Attachment kind.
    pub kind: AttachmentKind,
    /// The message is a sticker.
    pub sticker: bool,
    /// The file is the whole text of its message (`Attachment.long_text`): it is joined into
    /// the body instead of thumbnailed.
    pub long_text: bool,
}

impl std::fmt::Debug for DeriveJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeriveJob")
            .field("attachment", &self.attachment)
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

impl DeriveJob {
    /// Whether the file is a picture the core can thumbnail.
    pub(crate) fn is_picture(&self) -> bool {
        self.sticker
            || matches!(self.kind, AttachmentKind::Image | AttachmentKind::Gif)
            || self
                .mime
                .as_deref()
                .is_some_and(|m| m.starts_with("image/"))
    }
}

/// The attachment kind for a MIME type (outgoing files).
pub(crate) fn kind_for_mime(mime: &str) -> AttachmentKind {
    let mime = mime.to_ascii_lowercase();
    if mime == "image/gif" {
        AttachmentKind::Gif
    } else if mime.starts_with("image/") {
        AttachmentKind::Image
    } else if mime.starts_with("video/") {
        AttachmentKind::Video
    } else if mime.starts_with("audio/") {
        AttachmentKind::Audio
    } else {
        AttachmentKind::File
    }
}

/// How a selection of `files` is split into messages in a conversation with `features`
/// (`ConversationFeatures::plan`). Features the backend has not reported yet: every file in a
/// message of its own, which every client of every network shows, with the caption and the
/// quote on the first; the backend decides the rest.
pub(crate) fn plan_files(
    features: Option<&kourier_model::ConversationFeatures>,
    files: &[kourier_model::PlanFile<'_>],
    caption: Option<&kourier_model::Text>,
    reply: bool,
) -> kourier_model::FilePlan {
    match features {
        Some(f) => f.plan(files, caption, reply),
        None => kourier_model::FilePlan {
            lead_text: false,
            caption_messages: 0,
            messages: (0..files.len()).map(|i| vec![i]).collect(),
            reply_unsendable: false,
        },
    }
}

/// Deletes blob files (relative to `blob_dir`) whose rows a committed transaction dropped. A
/// missing file is fine. Blocking.
pub(crate) fn unlink(blob_dir: &Path, paths: &[PathBuf]) {
    for p in paths {
        if let Err(e) = std::fs::remove_file(blob_dir.join(p))
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(error = %e, "deleting a blob file failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_paths_round_trip() {
        let id = BlobId::new_random();
        let p = core_path(&id);
        assert!(p.starts_with(CORE_BLOBS));
        assert_eq!(core_id(&p), Some(id.clone()));
        assert_eq!(core_id(&id.relative_path()), None, "not a core path");
    }

    #[test]
    fn kinds_from_mime() {
        assert_eq!(kind_for_mime("image/GIF"), AttachmentKind::Gif);
        assert_eq!(kind_for_mime("image/jpeg"), AttachmentKind::Image);
        assert_eq!(kind_for_mime("video/mp4"), AttachmentKind::Video);
        assert_eq!(kind_for_mime("audio/ogg"), AttachmentKind::Audio);
        assert_eq!(kind_for_mime("application/pdf"), AttachmentKind::File);
    }

    #[test]
    fn jobs_hide_keys_and_make_targets_absolute() {
        let job = DownloadJob {
            attachment: AttachmentId(7),
            request: kourier_model::DownloadId(3),
            remote: BackendBlob::new(1, vec![1]),
            path: PathBuf::from("ab/abcd"),
            key: BlobKey::new([9; 32]),
            net_ids: vec!["plaintext:9".into()],
        };
        assert!(!format!("{job:?}").contains("9, 9"));
        match job.command(Path::new("/blobs")) {
            Command::DownloadAttachment {
                attachment,
                request,
                dest,
                ..
            } => {
                assert_eq!(attachment, AttachmentId(7));
                assert_eq!(request, kourier_model::DownloadId(3));
                assert_eq!(dest.path, PathBuf::from("/blobs/ab/abcd"));
            }
            other => panic!("unexpected {}", other.name()),
        }
    }
}
