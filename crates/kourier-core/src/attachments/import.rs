//! Outgoing files (§4.8 step 1, §5.8): each file the user picked is encrypted into the account's
//! blob store, with its metadata (kind, MIME type, size; dimensions, blurhash and a thumbnail for
//! pictures), before the messages are queued. The backend reads the blob and applies its own
//! transcoding (Signal: [`kourier_media::scale_to_level`]).
//!
//! There is no limit on how many files one send takes (Signal Desktop's 32 is its composer's
//! choice, not the network's): the core splits a selection into as many messages as the
//! network's packing rule needs (`ConversationFeatures::plan_files`). What a selection costs is
//! bounded per file, not per send: files are imported one after another and streamed into the
//! blob store in 64 KiB chunks; only a picture is held in memory, one at a time and at most
//! [`MAX_PICTURE_BYTES`], because its thumbnail and blurhash need the decoded image. The disk
//! space of the encrypted copies is the user's own to spend, as with any file manager.
//!
//! Every file is looked at before any is imported ([`inspect`]): a file the send would refuse
//! (unreadable, not a file, too large, a kind the chat does not take) is named with its
//! position, all of them at once, and nothing is copied for a send that will not happen. An
//! empty file is a file like any other and is sent: no server and no client refuses one
//! (Signal Desktop's composer checks only the upper size limit,
//! `SD/ts/state/ducks/composer.preload.ts` `preProcessAttachment`).
//!
//! The look also reads the file's format (its MIME type, from the first bytes), which the
//! network's packing rule may ask for (`FileAlbum::takes`).
//!
//! "Send as file": a picture, GIF or video the user wants to arrive as the file it is, not as
//! what a messenger makes of a picture, is imported as [`AttachmentKind::File`]. The bytes go
//! into the blob store as they are, nothing is decoded (no thumbnail, no blurhash, no
//! dimensions), the MIME type stays the true one and the name is kept. Backends re-encode by
//! kind and never touch a `File`, so the recipient gets the original, byte for byte.

use std::io::{Read, Seek};
use std::path::{Path, PathBuf};

use kourier_blob::{BlobStore, FileKey};

use super::core_path;
use kourier_model::{
    Attachment, AttachmentKind, AttachmentState, BlobKey, BlobRef, ConversationFeatures, PlanFile,
};
use zeroize::Zeroizing;

use crate::error::CoreError;
use crate::facade::{FileRefusal, RefusedFile};

/// Pictures up to this size are decoded for their thumbnail and blurhash; larger ones are sent
/// without. This is what bounds the memory of an import: one picture at a time.
const MAX_PICTURE_BYTES: u64 = 64 * 1024 * 1024;

/// Why an import stopped.
#[derive(Debug)]
pub(crate) enum ImportError {
    /// Files the send refuses, by position in the selection; nothing was imported.
    Refused(Vec<RefusedFile>),
    /// Anything else (the blob store failed); what was imported so far was removed again.
    Other(CoreError),
}

impl FileRefusal {
    /// The reason as the text of a `CoreError::InvalidInput` (log-safe: it never names the
    /// file).
    pub(crate) fn reason(self) -> &'static str {
        match self {
            Self::Unreadable => "a file could not be read",
            Self::NotAFile => "only files can be sent",
            Self::TooLarge { chat: false, .. } => "a file is larger than the network allows",
            Self::TooLarge { chat: true, .. } => "a file is larger than this chat allows",
            Self::KindNotAccepted { .. } => "this chat does not take this kind of file",
        }
    }
}

/// The error of a send some of whose files are refused: the reason of the first one.
pub(crate) fn refusal_error(refused: &[RefusedFile]) -> CoreError {
    CoreError::InvalidInput(
        refused
            .first()
            .map_or("a file cannot be sent", |r| r.reason.reason()),
    )
}

/// The type a file sent "as file" has for the packing rule: a generic file, whatever it holds.
const AS_FILE_MIME: &str = "application/octet-stream";

/// What the send reads from a file before it imports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Inspected {
    /// The kind it is sent as: what the content holds (the extension only when the content
    /// does not say), or `File` for a file the user sends as the file it is.
    pub kind: AttachmentKind,
    /// The kind of what it holds, whatever it is sent as.
    pub content: AttachmentKind,
    /// MIME type of what it holds.
    pub mime: String,
    /// Size on disk.
    pub size: u64,
}

impl Inspected {
    /// The file as the network's packing rule sees it. One sent as a file packs as a
    /// generic file: its picture format must not put it into an album.
    pub(crate) fn plan_file(&self) -> PlanFile<'_> {
        plan_file(self.kind, self.content, &self.mime)
    }
}

fn plan_file(kind: AttachmentKind, content: AttachmentKind, mime: &str) -> PlanFile<'_> {
    if kind == content {
        PlanFile::new(kind, mime)
    } else {
        PlanFile::new(kind, AS_FILE_MIME)
    }
}

/// How far an import is ([`import_files`] reports it while it copies).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ImportProgress {
    /// Files copied whole.
    pub files_done: usize,
    /// Bytes read from the user's files so far.
    pub bytes_done: u64,
}

/// One encrypted blob written by the import.
#[derive(Clone)]
pub(crate) struct Written {
    /// Relative to the account's blob directory.
    pub path: PathBuf,
    /// Its key.
    pub key: FileKey,
    /// Plaintext size.
    pub size: u64,
    /// MIME type.
    pub mime: String,
}

/// One imported file.
#[derive(Clone)]
pub(crate) struct Imported {
    /// The kind it is sent as ([`Inspected::kind`]).
    pub kind: AttachmentKind,
    /// The kind of what it holds.
    pub content: AttachmentKind,
    /// The file name the user picked (personal data, never logged).
    pub file_name: Option<String>,
    /// Display dimensions of a picture.
    pub dims: Option<(u32, u32)>,
    /// Blurhash of a picture.
    pub blurhash: Option<String>,
    /// The encrypted file.
    pub file: Written,
    /// The encrypted thumbnail of a picture.
    pub thumb: Option<Written>,
}

impl std::fmt::Debug for Imported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Imported")
            .field("kind", &self.kind)
            .field("mime", &self.file.mime)
            .field("size", &self.file.size)
            .finish_non_exhaustive()
    }
}

fn blob_ref(w: &Written) -> BlobRef {
    BlobRef {
        path: w.path.clone(),
        key: BlobKey::new(*w.key.as_bytes()),
    }
}

impl Imported {
    /// The attachment for the message body: `with_files` adds the blob references (the outbox
    /// request, which the backend reads); `core.db`'s message body keeps them in `files` only.
    pub(crate) fn attachment(&self, with_files: bool) -> Attachment {
        Attachment {
            kind: self.kind,
            mime: self.file.mime.clone(),
            size: self.file.size,
            file_name: self.file_name.clone(),
            dims: self.dims,
            blurhash: self.blurhash.clone(),
            state: AttachmentState::Local,
            local: with_files.then(|| blob_ref(&self.file)),
            thumb: if with_files {
                self.thumb.as_ref().map(blob_ref)
            } else {
                None
            },
            ..Attachment::default()
        }
    }

    /// The file as the network's packing rule sees it ([`Inspected::plan_file`]).
    pub(crate) fn plan_file(&self) -> PlanFile<'_> {
        plan_file(self.kind, self.content, &self.file.mime)
    }

    /// The blob files it wrote (relative paths).
    pub(crate) fn paths(&self) -> Vec<PathBuf> {
        let mut v = vec![self.file.path.clone()];
        v.extend(self.thumb.as_ref().map(|t| t.path.clone()));
        v
    }
}

/// An I/O error on the user's file. Never names the path: it is personal data (§9.1).
fn unreadable(_: std::io::Error) -> ImportError {
    ImportError::Refused(vec![RefusedFile {
        index: 0,
        reason: FileRefusal::Unreadable,
    }])
}

fn refused(reason: FileRefusal) -> ImportError {
    ImportError::Refused(vec![RefusedFile { index: 0, reason }])
}

/// Refuses a file above the network's send limit.
fn check_size(size: u64, max_bytes: Option<u64>) -> std::result::Result<(), FileRefusal> {
    match max_bytes {
        Some(max) if size > max => Err(FileRefusal::TooLarge { max, chat: false }),
        _ => Ok(()),
    }
}

/// What the conversation's features say about one file (§3.8: a kind absent from `files`
/// cannot be sent; `max_size` 0 means no per-kind limit), by the kind it is sent as. Unknown
/// features leave the decision to the backend.
pub(crate) fn check_features(
    features: Option<&ConversationFeatures>,
    kind: AttachmentKind,
    size: u64,
) -> std::result::Result<(), FileRefusal> {
    let Some(features) = features else {
        return Ok(());
    };
    let Some(limits) = features.files.get(&kind) else {
        return Err(FileRefusal::KindNotAccepted { kind });
    };
    if limits.max_size > 0 && size > limits.max_size {
        return Err(FileRefusal::TooLarge {
            max: limits.max_size,
            chat: true,
        });
    }
    Ok(())
}

/// Opens the user's file and reads its kind and format from the first bytes (blocking). A
/// FIFO, a directory or a device is refused before it is opened: opening a FIFO would block
/// until a writer appears. `as_file`: it is sent as the file it is.
fn open(
    path: &Path,
    max_bytes: Option<u64>,
    as_file: bool,
) -> std::result::Result<(std::fs::File, Inspected), FileRefusal> {
    let io = |_: std::io::Error| FileRefusal::Unreadable;
    let meta = std::fs::metadata(path).map_err(io)?;
    if !meta.is_file() {
        return Err(FileRefusal::NotAFile);
    }
    let size = meta.len();
    check_size(size, max_bytes)?;
    let mut file = std::fs::File::open(path).map_err(io)?;
    if !file.metadata().map_err(io)?.is_file() {
        // Replaced between the look and the open.
        return Err(FileRefusal::NotAFile);
    }
    let mut head = [0u8; 64];
    let n = read_up_to(&mut file, &mut head).map_err(io)?;
    file.rewind().map_err(io)?;
    let ext_mime = path
        .extension()
        .and_then(|e| e.to_str())
        .and_then(kourier_media::mime_from_extension);
    let mime = kourier_media::sniff_mime(&head[..n])
        .or(ext_mime)
        .unwrap_or("application/octet-stream")
        .to_owned();
    let content = super::kind_for_mime(&mime);
    let kind = if as_file {
        AttachmentKind::File
    } else {
        content
    };
    Ok((
        file,
        Inspected {
            kind,
            content,
            mime,
            size,
        },
    ))
}

/// Looks at one file as the send would, without copying it (blocking; reads 64 bytes): its
/// kind, format and size, or why the send refuses it.
pub(crate) fn inspect(
    path: &Path,
    max_bytes: Option<u64>,
    as_file: bool,
) -> std::result::Result<Inspected, FileRefusal> {
    open(path, max_bytes, as_file).map(|(_, inspected)| inspected)
}

/// Whether the file at `index` is sent as the file it is (`as_file` may be shorter than the
/// selection: missing entries are `false`).
pub(crate) fn sent_as_file(as_file: &[bool], index: usize) -> bool {
    as_file.get(index).copied().unwrap_or(false)
}

/// [`inspect`] for a whole selection, with the conversation's features applied: what was read
/// of every file that could be read (`None` otherwise) and every refusal, by position.
pub(crate) fn inspect_all(
    paths: &[PathBuf],
    as_file: &[bool],
    max_bytes: Option<u64>,
    features: Option<&ConversationFeatures>,
) -> (Vec<Option<Inspected>>, Vec<RefusedFile>) {
    let mut seen = Vec::with_capacity(paths.len());
    let mut refused = Vec::new();
    for (index, path) in paths.iter().enumerate() {
        let looked = inspect(path, max_bytes, sent_as_file(as_file, index));
        let checked = match &looked {
            Ok(i) => check_features(features, i.kind, i.size),
            Err(reason) => Err(*reason),
        };
        if let Err(reason) = checked {
            refused.push(RefusedFile { index, reason });
        }
        seen.push(looked.ok());
    }
    (seen, refused)
}

/// How many imported files are made durable together ([`make_durable`]).
const SYNC_BATCH: usize = 1024;

/// Makes the files of `imported` durable (blocking): every blob file is fsynced, then every
/// directory that got one. The copy itself writes without fsync, and the files of a batch
/// are synced together afterwards: what an fsync mostly waits for is the file system's
/// journal commit, and synced one right after the other the files of a batch share commits
/// where a sync after every file paid for one each, and for one more for its directory.
/// Measured back to back on this machine's disk while other builds kept it busy
/// (`tests/scale.rs` `measure_durable_import`, 1,000 small files, three rounds): 5.6, 12.0
/// and 8.4 ms per file as one batch against 11.9, 38.4 and 24.1 ms with a sync after every
/// file. Thumbnails are left out: they can be derived again (`derive::repair_thumbs`).
fn make_durable(blob_dir: &Path, imported: &[Imported]) -> std::io::Result<()> {
    let mut dirs = std::collections::BTreeSet::new();
    for i in imported {
        let path = blob_dir.join(&i.file.path);
        std::fs::File::open(&path)?.sync_all()?;
        if let Some(dir) = path.parent() {
            dirs.insert(dir.to_owned());
        }
    }
    // The shard directories, then the directory they are in (a shard may be new).
    let roots: std::collections::BTreeSet<PathBuf> = dirs
        .iter()
        .filter_map(|d| d.parent().map(Path::to_owned))
        .collect();
    for dir in dirs.iter().chain(&roots) {
        std::fs::File::open(dir)?.sync_all()?;
    }
    Ok(())
}

/// Imports `paths` into the blob store at `blob_dir` (blocking), one after another. Any number
/// of files: nothing here grows with the count but the list of what was written. Files larger
/// than `max_bytes` (the network's send limit) and anything that is not a regular file are
/// refused with their position. On any error, the blobs written so far are removed
/// again. `as_file` says which files are sent as the files they are ([`sent_as_file`]).
/// `progress` hears how far the copy is: once before the first file, after every chunk read
/// from a file, and after each file. Every file is durable when this returns
/// ([`make_durable`], in batches while the copy runs).
pub(crate) fn import_files(
    blob_dir: &Path,
    paths: &[PathBuf],
    as_file: &[bool],
    max_bytes: Option<u64>,
    progress: &mut dyn FnMut(ImportProgress),
) -> std::result::Result<Vec<Imported>, ImportError> {
    if paths.is_empty() {
        return Err(ImportError::Other(CoreError::InvalidInput(
            "no files to send",
        )));
    }
    let store =
        super::core_store(blob_dir).map_err(|e| ImportError::Other(CoreError::Blob(e.into())))?;
    let mut done: Vec<Imported> = Vec::with_capacity(paths.len());
    // How many of `done` are durable.
    let mut synced = 0;
    let mut at = ImportProgress::default();
    progress(at);
    for (index, p) in paths.iter().enumerate() {
        let before = at.bytes_done;
        let mut read = |n: u64| {
            at.bytes_done = before.saturating_add(n);
            progress(at);
        };
        match import_one(
            &store,
            p,
            max_bytes,
            sent_as_file(as_file, index),
            &mut read,
        ) {
            Ok(i) => {
                // What was read is what counts, also for a file that changed since the look.
                at.bytes_done = before.saturating_add(i.file.size);
                done.push(i);
                // A full batch, and at the end the rest, is made durable: the last report
                // (every file copied) comes when every file is.
                let last = index + 1 == paths.len();
                if last || done.len() - synced >= SYNC_BATCH {
                    if let Err(e) = make_durable(blob_dir, &done[synced..]) {
                        discard(blob_dir, &done);
                        return Err(ImportError::Other(CoreError::Blob(e.into())));
                    }
                    synced = done.len();
                }
                at.files_done = index + 1;
                progress(at);
            }
            Err(e) => {
                discard(blob_dir, &done);
                return Err(match e {
                    ImportError::Refused(mut files) => {
                        for f in &mut files {
                            f.index = index;
                        }
                        ImportError::Refused(files)
                    }
                    other => other,
                });
            }
        }
    }
    Ok(done)
}

/// A reader that says how many bytes went through it so far.
struct Counting<'a, R> {
    inner: R,
    read: u64,
    heard: &'a mut dyn FnMut(u64),
}

impl<R: Read> Read for Counting<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        if n > 0 {
            self.read = self
                .read
                .saturating_add(u64::try_from(n).unwrap_or(u64::MAX));
            (self.heard)(self.read);
        }
        Ok(n)
    }
}

/// Removes the blobs of an import whose message was not queued (blocking).
pub(crate) fn discard(blob_dir: &Path, imported: &[Imported]) {
    let paths: Vec<PathBuf> = imported.iter().flat_map(Imported::paths).collect();
    super::unlink(blob_dir, &paths);
}

/// One file. A refusal carries position 0: [`import_files`] puts the real one in. `read`
/// hears how many bytes of the file were read so far.
fn import_one(
    store: &BlobStore,
    path: &Path,
    max_bytes: Option<u64>,
    as_file: bool,
    read: &mut dyn FnMut(u64),
) -> std::result::Result<Imported, ImportError> {
    let blob = |e| ImportError::Other(CoreError::Blob(e));
    let (file, looked) = open(path, max_bytes, as_file).map_err(refused)?;
    let Inspected {
        kind,
        content,
        mime,
        size,
    } = looked;
    let mut file = Counting {
        inner: file,
        read: 0,
        heard: read,
    };
    let file_name = path.file_name().map(|n| n.to_string_lossy().into_owned());
    // A picture sent as a file is not decoded: nothing is derived from it, nothing of it is
    // held in memory, and the recipient gets the bytes the user picked.
    let picture =
        matches!(kind, AttachmentKind::Image | AttachmentKind::Gif) && size <= MAX_PICTURE_BYTES;
    if !picture {
        // Streamed: the blob writer reads and encrypts 64 KiB at a time. Not fsynced
        // here: `import_files` makes the files of a batch durable together.
        let (id, key, len) = store.import_volatile(&mut file).map_err(blob)?;
        // The file may have changed since it was measured: the limits hold for what was read.
        if let Err(reason) = check_size(len, max_bytes) {
            if let Err(e) = store.remove(&id) {
                tracing::warn!(error = %e, "removing an imported file failed");
            }
            return Err(refused(reason));
        }
        return Ok(Imported {
            kind,
            content,
            file_name,
            dims: None,
            blurhash: None,
            file: Written {
                path: core_path(&id),
                key,
                size: len,
                mime,
            },
            thumb: None,
        });
    }
    let mut bytes = Zeroizing::new(Vec::with_capacity(usize::try_from(size).unwrap_or(0)));
    // One byte more than the limit is enough to know a grown file is too large.
    let cap = max_bytes
        .unwrap_or(MAX_PICTURE_BYTES)
        .min(MAX_PICTURE_BYTES);
    (&mut file)
        .take(cap.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(unreadable)?;
    let read = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    if read > MAX_PICTURE_BYTES {
        // It grew past what is decoded in memory while it was read.
        return Err(refused(FileRefusal::Unreadable));
    }
    check_size(read, max_bytes).map_err(refused)?;
    let (id, key, len) = store.import_volatile(&mut bytes.as_slice()).map_err(blob)?;
    let file_written = Written {
        path: core_path(&id),
        key,
        size: len,
        mime,
    };
    let derived = match kourier_media::derive(&bytes) {
        Ok(d) => d,
        Err(e) => {
            tracing::debug!(error = %e, "an outgoing picture could not be decoded; sent as is");
            return Ok(Imported {
                kind,
                content,
                file_name,
                dims: None,
                blurhash: None,
                file: file_written,
                thumb: None,
            });
        }
    };
    // The thumbnail is not fsynced: it can be derived again from the file, which is
    // (`derive::repair_thumbs`).
    let thumb = match store.import_volatile(&mut derived.thumbnail.bytes.as_slice()) {
        Ok((tid, tkey, tlen)) => Some(Written {
            path: core_path(&tid),
            key: tkey,
            size: tlen,
            mime: derived.thumbnail.mime.to_owned(),
        }),
        Err(e) => {
            if let Err(e) = store.remove(&id) {
                tracing::warn!(error = %e, "removing an imported file failed");
            }
            return Err(blob(e));
        }
    };
    Ok(Imported {
        kind,
        content,
        file_name,
        dims: Some((derived.width, derived.height)),
        blurhash: Some(derived.blurhash),
        file: file_written,
        thumb,
    })
}

fn read_up_to(r: &mut impl Read, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kourier_media::{SampleFormat, sample_image};

    #[test]
    fn pictures_get_metadata_and_a_thumbnail_files_stream_through() {
        let dir = tempfile::tempdir().expect("tempdir");
        let blobs = dir.path().join("blobs");
        let src = dir.path().join("src");
        std::fs::create_dir_all(&src).expect("src");
        let jpeg = sample_image(900, 600, 4, SampleFormat::Jpeg).expect("jpeg");
        // A misleading extension: the content decides.
        std::fs::write(src.join("photo.png"), &jpeg).expect("write");
        std::fs::write(src.join("notes.txt"), b"plain words").expect("write");
        let got = import_files(
            &blobs,
            &[src.join("photo.png"), src.join("notes.txt")],
            &[],
            Some(10 << 20),
            &mut |_| {},
        )
        .expect("import");
        assert_eq!(got.len(), 2);
        let (pic, txt) = (&got[0], &got[1]);
        assert_eq!(
            (pic.kind, pic.file.mime.as_str(), pic.dims),
            (AttachmentKind::Image, "image/jpeg", Some((900, 600)))
        );
        assert!(pic.blurhash.is_some());
        let thumb = pic.thumb.as_ref().expect("thumb");
        assert_eq!(thumb.mime, "image/jpeg");
        assert_eq!(
            *kourier_blob::read_all(&blobs.join(&pic.file.path), &pic.file.key).expect("read"),
            jpeg
        );
        let t = kourier_blob::read_all(&blobs.join(&thumb.path), &thumb.key).expect("thumb");
        assert_eq!(kourier_media::probe(&t).expect("probe").width, 400);
        assert_eq!(
            (txt.kind, txt.file.mime.as_str(), txt.file.size),
            (AttachmentKind::File, "text/plain", 11)
        );
        assert_eq!(txt.file_name.as_deref(), Some("notes.txt"));
        // The request carries blob references, the stored body does not.
        assert!(pic.attachment(true).local.is_some() && pic.attachment(true).thumb.is_some());
        assert!(pic.attachment(false).local.is_none() && pic.attachment(false).thumb.is_none());
        assert!(!format!("{pic:?}").contains("photo"));
    }

    fn reason(e: ImportError) -> (usize, FileRefusal) {
        match e {
            ImportError::Refused(files) => {
                assert_eq!(files.len(), 1, "{files:?}");
                (files[0].index, files[0].reason)
            }
            ImportError::Other(e) => panic!("unexpected {e}"),
        }
    }

    #[test]
    fn refusals_name_the_file_and_leave_no_blobs_behind() {
        let dir = tempfile::tempdir().expect("tempdir");
        let blobs = dir.path().join("blobs");
        let ok = dir.path().join("ok.bin");
        std::fs::write(&ok, b"12345").expect("write");
        let big = dir.path().join("big.bin");
        std::fs::write(&big, vec![1u8; 100]).expect("write");
        let empty = dir.path().join("empty.bin");
        std::fs::write(&empty, b"").expect("write");
        let missing = dir.path().join("missing.bin");
        let too_large = FileRefusal::TooLarge {
            max: 50,
            chat: false,
        };
        for (paths, at, why) in [
            (vec![ok.clone(), big.clone()], 1, too_large),
            (vec![missing, ok.clone()], 0, FileRefusal::Unreadable),
            (
                vec![ok.clone(), dir.path().to_owned()],
                1,
                FileRefusal::NotAFile,
            ),
        ] {
            let got = reason(
                import_files(&blobs, &paths, &[], Some(50), &mut |_| {}).expect_err("refused"),
            );
            assert_eq!(got, (at, why));
            let store = crate::attachments::core_store(&blobs).expect("store");
            assert!(store.list().expect("list").is_empty(), "{why:?}");
        }
        assert!(matches!(
            import_files(&blobs, &[], &[], Some(50), &mut |_| {}),
            Err(ImportError::Other(CoreError::InvalidInput(
                "no files to send"
            )))
        ));
        // A FIFO is refused without blocking on its open (no writer ever comes).
        let fifo = dir.path().join("pipe");
        if std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .is_ok_and(|s| s.success())
        {
            let got =
                reason(import_files(&blobs, &[fifo], &[], None, &mut |_| {}).expect_err("fifo"));
            assert_eq!(got, (0, FileRefusal::NotAFile));
        }
        // The reasons keep the texts the UI's notices were written for, and name no file.
        assert_eq!(
            too_large.reason(),
            "a file is larger than the network allows"
        );
        assert_eq!(
            refusal_error(&[RefusedFile {
                index: 3,
                reason: FileRefusal::NotAFile
            }])
            .to_string(),
            "invalid request: only files can be sent"
        );
        // An empty file is no reason to refuse a send: it is imported like any other.
        let done =
            import_files(&blobs, &[ok, empty], &[], Some(50), &mut |_| {}).expect("imported");
        assert_eq!(
            done.iter().map(|i| i.file.size).collect::<Vec<_>>(),
            [5, 0],
            "the empty file is the second attachment"
        );
        assert_eq!(done[1].kind, AttachmentKind::File);
        let store = crate::attachments::core_store(&blobs).expect("store");
        assert_eq!(store.list().expect("list").len(), 2);
    }

    /// Desktop's composer stops at 32 attachments; nothing here does. A selection far beyond
    /// that is imported file by file, every one of them.
    #[test]
    fn any_number_of_files_is_imported() {
        let dir = tempfile::tempdir().expect("tempdir");
        let blobs = dir.path().join("blobs");
        let paths: Vec<PathBuf> = (0..100)
            .map(|i| {
                let p = dir.path().join(format!("f{i}.bin"));
                std::fs::write(&p, format!("file number {i}")).expect("write");
                p
            })
            .collect();
        let mut heard = Vec::new();
        let got = import_files(&blobs, &paths, &[], Some(1 << 20), &mut |p| heard.push(p))
            .expect("import");
        assert_eq!(got.len(), 100);
        // The progress starts at nothing, never goes back and ends at everything.
        let total: u64 = got.iter().map(|f| f.file.size).sum();
        assert_eq!(heard.first(), Some(&ImportProgress::default()));
        assert_eq!(
            heard.last(),
            Some(&ImportProgress {
                files_done: 100,
                bytes_done: total
            })
        );
        assert!(
            heard
                .windows(2)
                .all(|w| w[0].files_done <= w[1].files_done && w[0].bytes_done <= w[1].bytes_done),
            "{heard:?}"
        );
        for (i, f) in got.iter().enumerate() {
            let text =
                kourier_blob::read_all(&blobs.join(&f.file.path), &f.file.key).expect("read");
            assert_eq!(*text, format!("file number {i}").into_bytes(), "in order");
        }
    }

    #[test]
    fn a_selection_is_inspected_whole_before_anything_is_copied() {
        use kourier_model::FileLimits;
        let dir = tempfile::tempdir().expect("tempdir");
        let jpeg = sample_image(40, 30, 1, SampleFormat::Jpeg).expect("jpeg");
        // The content decides the kind, not the name.
        let photo = dir.path().join("photo.txt");
        std::fs::write(&photo, &jpeg).expect("write");
        let notes = dir.path().join("notes.txt");
        std::fs::write(&notes, b"plain words").expect("write");
        let big = dir.path().join("big.bin");
        std::fs::write(&big, vec![7u8; 400]).expect("write");
        let missing = dir.path().join("missing.bin");
        let features = ConversationFeatures {
            files: [
                (
                    AttachmentKind::Image,
                    FileLimits {
                        max_size: 0,
                        caption: true,
                    },
                ),
                (
                    AttachmentKind::File,
                    FileLimits {
                        max_size: 300,
                        caption: true,
                    },
                ),
            ]
            .into(),
            ..ConversationFeatures::default()
        };
        let paths = [photo.clone(), notes.clone(), big, missing, photo.clone()];
        let (seen, refused) = inspect_all(&paths, &[], Some(10_000), Some(&features));
        let kinds: Vec<Option<AttachmentKind>> =
            seen.iter().map(|i| i.as_ref().map(|i| i.kind)).collect();
        assert_eq!(
            seen[0].as_ref().map(|i| (i.mime.as_str(), i.content)),
            Some(("image/jpeg", AttachmentKind::Image)),
            "the format is read with the kind"
        );
        assert_eq!(
            kinds,
            [
                Some(AttachmentKind::Image),
                Some(AttachmentKind::File),
                Some(AttachmentKind::File),
                None,
                Some(AttachmentKind::Image)
            ]
        );
        // Every refused file at once, each with its position and its own reason.
        assert_eq!(
            refused,
            [
                RefusedFile {
                    index: 2,
                    reason: FileRefusal::TooLarge {
                        max: 300,
                        chat: true
                    }
                },
                RefusedFile {
                    index: 3,
                    reason: FileRefusal::Unreadable
                },
            ]
        );
        // A chat that takes no pictures refuses them by kind; unknown features refuse nothing.
        let mut no_pictures = features.clone();
        no_pictures.files.remove(&AttachmentKind::Image);
        let (_, refused) = inspect_all(&[photo.clone(), notes], &[], None, Some(&no_pictures));
        assert_eq!(
            refused,
            [RefusedFile {
                index: 0,
                reason: FileRefusal::KindNotAccepted {
                    kind: AttachmentKind::Image
                }
            }]
        );
        assert!(
            inspect_all(std::slice::from_ref(&photo), &[], None, None)
                .1
                .is_empty()
        );
        // Sent as a file, the picture is a generic file for the chat's rules: a chat that
        // takes no pictures takes it, and the size limit of files applies to it.
        no_pictures.files.insert(
            AttachmentKind::File,
            FileLimits {
                max_size: 0,
                caption: true,
            },
        );
        let (seen, refused) = inspect_all(
            std::slice::from_ref(&photo),
            &[true],
            None,
            Some(&no_pictures),
        );
        assert!(refused.is_empty(), "{refused:?}");
        let as_file = seen[0].as_ref().expect("read");
        assert_eq!(
            (as_file.kind, as_file.content, as_file.mime.as_str()),
            (AttachmentKind::File, AttachmentKind::Image, "image/jpeg")
        );
        assert_eq!(
            as_file.plan_file(),
            PlanFile::new(AttachmentKind::File, "application/octet-stream"),
            "its picture format never puts it into an album"
        );
        let mut tight = features.clone();
        tight.files.insert(
            AttachmentKind::File,
            FileLimits {
                max_size: 10,
                caption: true,
            },
        );
        let (_, refused) = inspect_all(&[photo], &[true], None, Some(&tight));
        assert_eq!(
            refused,
            [RefusedFile {
                index: 0,
                reason: FileRefusal::TooLarge {
                    max: 10,
                    chat: true
                }
            }]
        );
    }

    /// "Send as file": the picture is stored byte for byte, with its real type and name, as a
    /// generic file; nothing is decoded and no thumbnail is made.
    #[test]
    fn a_picture_sent_as_a_file_keeps_its_bytes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let blobs = dir.path().join("blobs");
        let jpeg = sample_image(900, 600, 4, SampleFormat::Jpeg).expect("jpeg");
        let photo = dir.path().join("holiday.jpg");
        std::fs::write(&photo, &jpeg).expect("write");
        let got = import_files(
            &blobs,
            &[photo.clone(), photo],
            &[true],
            Some(10 << 20),
            &mut |_| {},
        )
        .expect("import");
        let (original, picture) = (&got[0], &got[1]);
        assert_eq!(
            (original.kind, original.content, original.file.mime.as_str()),
            (AttachmentKind::File, AttachmentKind::Image, "image/jpeg")
        );
        assert!(original.thumb.is_none() && original.dims.is_none() && original.blurhash.is_none());
        assert_eq!(original.file_name.as_deref(), Some("holiday.jpg"));
        assert_eq!(
            *kourier_blob::read_all(&blobs.join(&original.file.path), &original.file.key)
                .expect("read"),
            jpeg
        );
        let a = original.attachment(true);
        assert_eq!(
            (a.kind, a.mime.as_str(), a.size),
            (AttachmentKind::File, "image/jpeg", jpeg.len() as u64)
        );
        // The same file without the choice is a picture with its thumbnail (the missing
        // entry of `as_file` is `false`).
        assert_eq!(picture.kind, AttachmentKind::Image);
        assert!(picture.thumb.is_some() && picture.dims == Some((900, 600)));
        assert_eq!(
            picture.plan_file(),
            PlanFile::new(AttachmentKind::Image, "image/jpeg")
        );
    }
}
