//! Receiving a file (W7): the `DownloadAttachment` command.
//!
//! A pointer ([`MediaPointer`], kept in the attachment's `remote`) becomes upstream's [`DownloadParams`]; upstream
//! fetches, verifies and decrypts; the plaintext goes straight into the core's sealed blob through [`BlobSink`], so no
//! plaintext file ever exists. The target appears only when the whole file verified: `BlobWriter` writes `<path>.tmp` and
//! renames it at [`BlobSink::finish`].
//!
//! The answer is one durable `AttachmentDone` in one `core_outbox` zone. A kill before that commit loses nothing: the core
//! sends an unanswered request again once the backend is `Online`, with the same target.
//!
//! The stated length bounds the file (a sender who states a small file and uploads a large one is refused at the stated
//! length, so the auto-download policy cannot be walked around), and a failure names no path, key or URL.

use std::io::{self, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use kourier_backend_api::{BackendEvent, BlobTarget, Clock, Envelope};
use kourier_blob::{BlobWriter, FileKey};
use kourier_model::{
    AttachmentId, AttachmentMeta, BackendBlob, BackendError, DownloadId, ErrorKind,
};
use tokio::sync::{Notify, Semaphore, mpsc};
use wacore::download::{DownloadWriter, MediaType};
use whatsapp_rust::download::DownloadParams;

use crate::media::{MediaPointer, unhex};
use crate::outbox;
use crate::session::{DownloadFailure, Session};
use crate::store::KourierStore;

/// Files fetched at the same time. The rest wait in line.
pub(crate) const SLOTS: usize = 3;
/// The bound of a file whose message states no length (WhatsApp's own limit is 2 GiB).
const MAX_UNSTATED: u64 = 2 * 1024 * 1024 * 1024;
/// A progress report at most once per this many plaintext bytes (and per this share of the file: [`REPORT_SHARE`]).
const REPORT_BYTES: u64 = 256 * 1024;
/// Reports per file, at most.
const REPORT_SHARE: u64 = 50;

/// What one download needs of the actor.
pub(crate) struct Parts {
    pub(crate) store: KourierStore,
    /// `None`: no client runs (the request is answered as something to try again).
    pub(crate) session: Option<Arc<dyn Session>>,
    pub(crate) events: mpsc::Sender<Envelope>,
    pub(crate) wake: Arc<Notify>,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) slots: Arc<Semaphore>,
}

/// Runs one `DownloadAttachment` to its durable answer.
pub(crate) async fn run(
    parts: Parts,
    attachment: AttachmentId,
    request: DownloadId,
    remote: BackendBlob,
    dest: BlobTarget,
) {
    let result = fetch(&parts, attachment, &remote, dest).await;
    if let Err(e) = &result {
        tracing::warn!(kind = ?e.kind, "a file did not download");
    }
    let answer = BackendEvent::AttachmentDone {
        attachment,
        request: Some(request),
        result,
    };
    let now = i64::try_from(parts.clock.now_ms()).unwrap_or(i64::MAX);
    match outbox::append_download_answer(&parts.store, answer, now).await {
        Ok(_) => parts.wake.notify_one(),
        Err(e) => tracing::warn!(error = %e, "a download request could not be answered"),
    }
}

async fn fetch(
    parts: &Parts,
    attachment: AttachmentId,
    remote: &BackendBlob,
    dest: BlobTarget,
) -> Result<AttachmentMeta, BackendError> {
    let pointer = MediaPointer::from_blob(remote).ok_or_else(|| invalid("no pointer"))?;
    let params = params_of(&pointer).ok_or_else(|| invalid("an incomplete pointer"))?;
    let Some(session) = parts.session.clone() else {
        return Err(BackendError::transient("not connected"));
    };
    let _slot = parts
        .slots
        .acquire()
        .await
        .map_err(|_| BackendError::transient("the account is stopping"))?;

    let total = pointer.file_length;
    report(&parts.events, attachment, 0, total);
    let over = Arc::new(AtomicBool::new(false));
    let target = Target {
        path: dest.path,
        key: FileKey::from_bytes(*dest.key.expose()),
        limit: if total == 0 { MAX_UNSTATED } else { total },
        over: over.clone(),
        events: parts.events.clone(),
        attachment,
        total,
    };
    let sink = tokio::task::spawn_blocking(move || BlobSink::create(target))
        .await
        .map_err(|_| BackendError::internal("the file could not be opened"))?
        .map_err(|e| BackendError::internal(format!("opening the file: {}", e.kind())))?;

    let sink = session.download(params, sink).await.map_err(|failure| {
        if over.load(Ordering::Relaxed) {
            BackendError::new(ErrorKind::TooLarge, "the file is larger than it said")
        } else {
            failure.into_backend()
        }
    })?;
    let size = tokio::task::spawn_blocking(move || sink.finish())
        .await
        .map_err(|_| BackendError::internal("the file could not be finished"))?
        .map_err(|e| BackendError::internal(format!("finishing the file: {}", e.kind())))?;
    report(&parts.events, attachment, size, size);
    Ok(AttachmentMeta {
        size,
        mime: pointer.mimetype.clone(),
        dims: pointer.width.zip(pointer.height),
        duration_ms: pointer.seconds.map(|s| s.saturating_mul(1000)),
    })
}

fn invalid(what: &'static str) -> BackendError {
    BackendError::new(ErrorKind::InvalidInput, what)
}

/// The request for the file `p` names; `None` when it lacks what a download needs (a key, a hash) or names a type
/// Kourier does not fetch.
pub(crate) fn params_of(p: &MediaPointer) -> Option<DownloadParams> {
    let media_type = match p.media_type.as_str() {
        "image" => MediaType::Image,
        // A video note is sealed and stored as a video.
        "video" | "ptv" => MediaType::Video,
        "audio" => MediaType::Audio,
        "document" => MediaType::Document,
        "sticker" => MediaType::Sticker,
        _ => return None,
    };
    DownloadParams::encrypted(
        p.direct_path.as_str(),
        &unhex(&p.media_key)?,
        &unhex(p.file_sha256.as_deref()?)?,
        &unhex(p.file_enc_sha256.as_deref()?)?,
        p.file_length,
        media_type,
    )
    .ok()
}

/// Reports progress; never waits (a full channel skips this report).
fn report(events: &mpsc::Sender<Envelope>, attachment: AttachmentId, done: u64, total: u64) {
    let _ = events.try_send(Envelope::ephemeral(BackendEvent::DownloadProgress {
        attachment,
        done,
        total,
    }));
}

/// Where a [`BlobSink`] writes and whom it tells.
pub(crate) struct Target {
    pub(crate) path: PathBuf,
    pub(crate) key: FileKey,
    /// The most plaintext this file may be.
    pub(crate) limit: u64,
    /// Set when the file went past `limit`.
    pub(crate) over: Arc<AtomicBool>,
    pub(crate) events: mpsc::Sender<Envelope>,
    pub(crate) attachment: AttachmentId,
    /// The stated length (for the reports; 0: not stated).
    pub(crate) total: u64,
}

/// Upstream's download destination over a sealed blob.
///
/// A blob is written once from its start, so this is not a real `Seek`: upstream empties its writer before every attempt
/// on a host (`truncate(0)`, which starts the blob over) and rewinds it after one that verified, which it does for a
/// caller that reads the file back. Those are all this answers; any other position is refused, and so is a write
/// after a rewind that did not empty the file, so that two attempts can never end up in one file.
pub(crate) struct BlobSink {
    writer: Option<BlobWriter>,
    target: Target,
    len: u64,
    reported: u64,
    rewound: bool,
}

impl BlobSink {
    /// Starts the blob (its directory is made, 0700). Blocking.
    pub(crate) fn create(target: Target) -> io::Result<Self> {
        use std::os::unix::fs::DirBuilderExt as _;
        if let Some(parent) = target.path.parent() {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(parent)?;
        }
        let writer = BlobWriter::create(&target.path, &target.key).map_err(io::Error::other)?;
        Ok(Self {
            writer: Some(writer),
            target,
            len: 0,
            reported: 0,
            rewound: false,
        })
    }

    /// Seals the file and puts it in place; its plaintext length. Blocking.
    pub(crate) fn finish(mut self) -> io::Result<u64> {
        self.writer
            .take()
            .ok_or_else(|| io::Error::other("no file"))?
            .finish()
            .map_err(io::Error::other)
    }

    /// How far the file must have come since the last report.
    fn report_step(&self) -> u64 {
        (self.target.total / REPORT_SHARE).max(REPORT_BYTES)
    }
}

impl Write for BlobSink {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if self.rewound {
            return Err(io::Error::other("a write after a rewind"));
        }
        let writer = self
            .writer
            .as_mut()
            .ok_or_else(|| io::Error::other("no file"))?;
        if self.len.saturating_add(data.len() as u64) > self.target.limit {
            self.target.over.store(true, Ordering::Relaxed);
            return Err(io::Error::other("the file is larger than it said"));
        }
        let n = writer.write(data)?;
        self.len += n as u64;
        if self.len - self.reported >= self.report_step() {
            self.reported = self.len;
            report(
                &self.target.events,
                self.target.attachment,
                self.len,
                self.target.total,
            );
        }
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Seek for BlobSink {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        match pos {
            SeekFrom::Start(0) => {
                self.rewound = self.len > 0;
                Ok(0)
            }
            SeekFrom::Current(0) => Ok(if self.rewound { 0 } else { self.len }),
            _ => Err(io::Error::from(io::ErrorKind::Unsupported)),
        }
    }
}

impl DownloadWriter for BlobSink {
    fn truncate(&mut self, len: u64) -> io::Result<()> {
        if len != 0 {
            return Err(io::Error::from(io::ErrorKind::Unsupported));
        }
        // The old temporary file goes first: the new writer takes the same name.
        self.writer = None;
        self.len = 0;
        self.reported = 0;
        self.rewound = false;
        self.writer = Some(
            BlobWriter::create(&self.target.path, &self.target.key).map_err(io::Error::other)?,
        );
        Ok(())
    }
}

impl DownloadFailure {
    fn into_backend(self) -> BackendError {
        match self {
            Self::NoClient => BackendError::transient("not connected"),
            Self::Unreachable => BackendError::transient("the file's hosts did not answer"),
            Self::Gone => BackendError::new(ErrorKind::Expired, "the file is no longer there"),
            Self::Sink => BackendError::internal("the file could not be written"),
            Self::Invalid => invalid("the file cannot be asked for"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use kourier_blob::read_all;

    fn sink(dir: &std::path::Path, limit: u64) -> (BlobSink, PathBuf, FileKey, Arc<AtomicBool>) {
        let path = dir.join("sub").join("file");
        let (events, _rx) = mpsc::channel(4);
        let key = FileKey::from_bytes([7; 32]);
        let over = Arc::new(AtomicBool::new(false));
        let sink = BlobSink::create(Target {
            path: path.clone(),
            key: FileKey::from_bytes([7; 32]),
            limit,
            over: over.clone(),
            events,
            attachment: AttachmentId(1),
            total: limit,
        })
        .expect("created");
        (sink, path, key, over)
    }

    #[test]
    fn a_second_attempt_leaves_only_its_own_bytes() {
        let dir = tempfile::tempdir().expect("dir");
        let (mut s, path, key, _) = sink(dir.path(), 64);
        s.write_all(b"from the first host").expect("written");
        s.truncate(0).expect("emptied");
        s.rewind().expect("rewound");
        s.write_all(b"second").expect("written");
        s.rewind().expect("the verified rewind");
        assert_eq!(s.finish().expect("finished"), 6);
        assert_eq!(&*read_all(&path, &key).expect("read"), b"second");
    }

    #[test]
    fn nothing_is_in_place_before_the_file_is_finished() {
        let dir = tempfile::tempdir().expect("dir");
        let (mut s, path, _, _) = sink(dir.path(), 64);
        s.write_all(b"partial").expect("written");
        assert!(!path.exists());
        drop(s);
        assert!(!path.exists());
        let left = std::fs::read_dir(path.parent().expect("parent"))
            .expect("listed")
            .count();
        assert_eq!(left, 0, "the temporary file went with the sink");
    }

    #[test]
    fn a_file_past_its_stated_length_is_refused() {
        let dir = tempfile::tempdir().expect("dir");
        let (mut s, _, _, over) = sink(dir.path(), 4);
        s.write_all(b"1234").expect("up to the limit");
        assert!(s.write_all(b"5").is_err());
        assert!(over.load(Ordering::Relaxed));
    }

    #[test]
    fn two_attempts_cannot_run_together_in_one_file() {
        let dir = tempfile::tempdir().expect("dir");
        let (mut s, ..) = sink(dir.path(), 64);
        s.write_all(b"first").expect("written");
        s.rewind().expect("rewound without emptying");
        assert!(s.write_all(b"second").is_err());
    }

    #[test]
    fn only_the_positions_upstream_uses_are_answered() {
        let dir = tempfile::tempdir().expect("dir");
        let (mut s, ..) = sink(dir.path(), 64);
        assert!(s.seek(SeekFrom::Start(3)).is_err());
        assert!(s.seek(SeekFrom::End(0)).is_err());
        assert!(s.truncate(3).is_err());
    }

    fn pointer() -> MediaPointer {
        MediaPointer {
            media_type: "ptv".to_owned(),
            direct_path: "/v/t62/abc".to_owned(),
            media_key: "11".repeat(32),
            file_sha256: Some("22".repeat(32)),
            file_enc_sha256: Some("33".repeat(32)),
            file_length: 9,
            ..MediaPointer::default()
        }
    }

    #[test]
    fn a_whole_pointer_is_a_request() {
        assert!(params_of(&pointer()).is_some());
    }

    #[test]
    fn a_pointer_without_what_verifies_the_file_is_no_request() {
        let mut p = pointer();
        p.file_enc_sha256 = None;
        assert!(params_of(&p).is_none());
        let mut p = pointer();
        p.file_sha256 = Some("zz".to_owned());
        assert!(params_of(&p).is_none());
        let mut p = pointer();
        p.media_key = "11".to_owned();
        assert!(params_of(&p).is_none());
        let mut p = pointer();
        p.media_type = "history".to_owned();
        assert!(params_of(&p).is_none());
    }
}
