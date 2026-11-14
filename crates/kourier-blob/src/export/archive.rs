//! The export archive: a ZIP written into a folder the user chose (architecture §5.8.3), the
//! second form of [`super`] beside the single copy of [`super::save_to`].
//!
//! The staging, the `fsync`s and the publishing are the parent module's (the open folder fd,
//! `O_TMPFILE` or the hidden `.kourier-<random>.part`, [`SaveMode`]); the differences:
//!
//! - The staged file is created `0o600`: an archive holds a whole conversation in plaintext.
//! - Files (`Source::Blob`) are STORED (media is compressed already) and documents
//!   (`Source::Bytes`) are deflated; an entry of [`ArchiveOptions::zip64_from`] bytes or more
//!   is written as ZIP64 (the archive itself and 65,535+ entries are handled by the zip crate).
//!   Entry permissions are `0o644`, never an executable bit; every entry carries the export
//!   time (local time, plus the extended timestamp in UTC).
//! - ENTRY NAMES are checked here ([`check_entry_name`]) before anything is written: a top-level
//!   name or `files/<one component>`, each component non-empty, at most 255 bytes, without `/`,
//!   `\`, NUL or any other control character (C0, DEL, C1), bidi and zero-width characters
//!   (U+061C, U+200B, U+200E, U+200F, U+202A-202E, U+2060-2064, U+2066-2069, U+FEFF), the characters Windows
//!   refuses (`: * ? " < > |`), a trailing `.` or space, the reserved device names (CON, PRN,
//!   AUX, NUL, COM1-9, LPT1-9, with or without an extension), not `.` or `..`; every name
//!   unique. The joiners U+200C and U+200D stay allowed (Persian words, emoji sequences). A blob entry is `files/<name>` and a document has no `/` (M3). NFC and NFD
//!   spellings of one name are two names here (a known limit: no normalisation crate).
//!   One bad name refuses the whole archive ([`SaveError::BadName`]).
//! - A file streams from its blob in chunks and is hashed (SHA-256) on the way. A source that
//!   fails (does not open, a chunk that does not authenticate, a truncated blob) has its entry
//!   removed again (`ZipWriter::abort_file`), is reported in [`ArchiveReport::failed`], and the
//!   archive goes on.
//! - `cancel` is looked at between chunks; a cancelled or failed archive leaves nothing under
//!   the final name and no `.part` file.
//! - The manifest is the LAST entry: the caller's `manifest` closure gets the entries written
//!   so far and the failed names and returns the manifest's name and bytes.
//! - The log says how many entries and bytes, never a name or a path (§9.1).

use super::{
    Part, SaveError, SaveMode, Staging, check_name, classify, exists, open_dir, publish_anonymous,
    publish_named, stage, sync, sync_dir,
};
use crate::{BlobReader, FileKey};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::ffi::CString;
use std::io::{self, BufWriter, Read, Seek, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use zeroize::Zeroizing;
use zip::write::{FullFileOptions, SimpleFileOptions};
use zip::{CompressionMethod, ZipWriter};

/// An entry of this many bytes or more is written as ZIP64 (a little below 4 GiB, as the
/// folder packer does).
pub const ZIP64_FROM: u64 = 0xFF00_0000;

/// The "extended timestamp" extra field (Info-ZIP).
const EXTENDED_TIMESTAMP: u16 = 0x5455;

/// Where an entry's bytes come from.
pub enum Source {
    /// A file of the store: decrypted and streamed; `size` is the plaintext size the store
    /// states (it decides ZIP64 only; the entry's real size is what was read).
    Blob {
        /// The encrypted file.
        path: PathBuf,
        /// Its key.
        key: FileKey,
        /// The plaintext size the store states.
        size: u64,
    },
    /// A document built by the caller (`index.html`, `messages.jsonl`).
    Bytes(Vec<u8>),
}

/// One entry to write.
pub struct ArchiveItem {
    /// The entry name ([`check_entry_name`]).
    pub name: String,
    /// Its bytes.
    pub source: Source,
}

/// What was written for one entry. `Debug` shows only the size (the name is the sender's).
#[derive(Clone, PartialEq, Eq)]
pub struct EntryInfo {
    /// The entry name.
    pub name: String,
    /// Its size in bytes.
    pub size: u64,
    /// The SHA-256 of its bytes, lower-case hex.
    pub sha256: String,
}

/// What an archive holds after it was published. `Debug` shows counts and sizes only.
#[derive(Clone, PartialEq, Eq)]
pub struct ArchiveReport {
    /// Every entry in the archive in order, the manifest last.
    pub entries: Vec<EntryInfo>,
    /// The names of the file entries that could not be read to their end and are not in it.
    pub failed: Vec<String>,
    /// The archive's length on the disk.
    pub bytes: u64,
}

impl std::fmt::Debug for EntryInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EntryInfo")
            .field("size", &self.size)
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for ArchiveReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ArchiveReport")
            .field("entries", &self.entries.len())
            .field("failed", &self.failed.len())
            .field("bytes", &self.bytes)
            .finish()
    }
}

/// How an archive is written.
pub struct ArchiveOptions<'a> {
    /// Replace or refuse a taken name.
    pub mode: SaveMode,
    /// The staging route (the tests force [`Staging::Named`]).
    pub staging: Staging,
    /// Stops the archive between chunks ([`SaveError::Cancelled`]).
    pub cancel: &'a AtomicBool,
    /// The size from which an entry is ZIP64 ([`ZIP64_FROM`]; a test lowers it).
    pub zip64_from: u64,
    /// The time every entry carries.
    pub modified: SystemTime,
    /// How long the writer waits before each FILE entry (zero: no wait). Only a window test
    /// sets it, to hold an export running; the wait is in slices, so a cancel is seen at once,
    /// and no lock or transaction is held while it lasts.
    pub entry_pause: std::time::Duration,
}

impl<'a> ArchiveOptions<'a> {
    /// The defaults: no replace, automatic staging, ZIP64 from [`ZIP64_FROM`], the time now.
    pub fn new(cancel: &'a AtomicBool) -> Self {
        Self {
            mode: SaveMode::NoReplace,
            staging: Staging::Auto,
            cancel,
            zip64_from: ZIP64_FROM,
            modified: SystemTime::now(),
            entry_pause: std::time::Duration::ZERO,
        }
    }
}

/// Whether `name` can be an entry name of the archive: a top-level name or `files/<name>`,
/// each component one valid file name (see the module's rules).
pub fn check_entry_name(name: &str) -> Result<(), SaveError> {
    /// The invisible format characters that reorder or hide text (bidi controls, zero-width).
    fn invisible(ch: char) -> bool {
        matches!(ch,
            '\u{061C}' | '\u{200B}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}' | '\u{2066}'..='\u{2069}' | '\u{FEFF}')
    }
    /// A device name Windows reserves, with or without an extension.
    fn reserved(c: &str) -> bool {
        let stem = c.split('.').next().unwrap_or(c).trim_end_matches(' ');
        let up = stem.to_ascii_uppercase();
        matches!(up.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || ((up.starts_with("COM") || up.starts_with("LPT"))
                && up.len() == 4
                && matches!(up.as_bytes()[3], b'1'..=b'9'))
    }
    fn component(c: &str) -> bool {
        !c.is_empty()
            && c.len() <= super::MAX_NAME_BYTES
            && c != "."
            && c != ".."
            && !c.ends_with('.')
            && !c.ends_with(' ')
            && !reserved(c)
            && !c.chars().any(|ch| {
                matches!(ch, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|')
                    || ch.is_control()
                    || invisible(ch)
            })
    }
    let ok = match name.split_once('/') {
        None => component(name),
        Some(("files", rest)) => component(rest),
        Some(_) => false,
    };
    ok.then_some(()).ok_or(SaveError::BadName)
}

/// What makes the manifest of [`write_archive`]: given the entries written and the names of the
/// files that failed, it returns the manifest's entry name and bytes.
pub type ManifestMaker<'a> = dyn FnMut(&[EntryInfo], &[String]) -> (String, Vec<u8>) + 'a;

/// Waits [`ArchiveOptions::entry_pause`] in short slices; a cancel ends it.
fn pause_before_entry(options: &ArchiveOptions<'_>) -> Result<(), SaveError> {
    let end = std::time::Instant::now() + options.entry_pause;
    while std::time::Instant::now() < end {
        if options.cancel.load(Ordering::Relaxed) {
            return Err(SaveError::Cancelled);
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    Ok(())
}

/// Writes the archive `dir/name` from `items` (in order), then the manifest `manifest` makes
/// from the entries and the failed names; `progress` gets the bytes of content done so far.
pub fn write_archive(
    dir: &Path,
    name: &str,
    items: Vec<ArchiveItem>,
    options: &ArchiveOptions<'_>,
    progress: &mut dyn FnMut(u64),
    manifest: &mut ManifestMaker<'_>,
) -> Result<ArchiveReport, SaveError> {
    check_name(name)?;
    let mut seen: HashSet<String> = HashSet::new();
    for item in &items {
        check_entry_name(&item.name)?;
        // The place follows the kind: files under `files/`, documents at the top.
        let in_files = item.name.starts_with("files/");
        if in_files != matches!(item.source, Source::Blob { .. }) {
            return Err(SaveError::BadName);
        }
        if !seen.insert(item.name.clone()) {
            return Err(SaveError::BadName);
        }
    }
    let target = CString::new(name).map_err(|_| SaveError::BadName)?;
    let dirfd = open_dir(dir)?;
    if options.mode == SaveMode::NoReplace && exists(&dirfd, &target)? {
        return Err(SaveError::Exists);
    }
    let (file, part) = stage(&dirfd, options.staging, 0o600)?;
    // `&File` is `Write + Seek`: the staged file stays reachable beside the writer, so an
    // aborted entry's bytes can be cut off at once (H1).
    let mut zip = ZipWriter::new(BufWriter::new(&file));
    let mut entries: Vec<EntryInfo> = Vec::new();
    let mut failed: Vec<String> = Vec::new();
    let mut done = 0u64;
    let mut buf = Zeroizing::new(vec![0u8; 64 * 1024]);
    for item in items {
        if options.cancel.load(Ordering::Relaxed) {
            return Err(SaveError::Cancelled);
        }
        match item.source {
            Source::Bytes(bytes) => {
                let info = add_bytes(&mut zip, &item.name, &bytes, options)?;
                done += info.size;
                progress(done);
                entries.push(info);
            }
            Source::Blob { path, key, size } => {
                pause_before_entry(options)?;
                let Ok(mut reader) = BlobReader::open(&path, &key) else {
                    failed.push(item.name);
                    continue;
                };
                // The ciphertext bounds the plaintext: a stated size below the real one
                // cannot keep an entry of 4 GiB or more out of ZIP64 (L1).
                let real = std::fs::metadata(&path).map_or(0, |m| m.len());
                // Where the entry's header will start (the buffer is flushed first).
                flush_zip(&mut zip)?;
                let entry_start = (&file).stream_position().map_err(write_error)?;
                zip.start_file(
                    &item.name,
                    entry_options(options, CompressionMethod::Stored, size.max(real)),
                )
                .map_err(zip_error)?;
                let mut hasher = Sha256::new();
                let mut total = 0u64;
                let complete = loop {
                    if options.cancel.load(Ordering::Relaxed) {
                        return Err(SaveError::Cancelled);
                    }
                    match reader.read(&mut buf) {
                        Ok(0) => break true,
                        Ok(n) => {
                            hasher.update(&buf[..n]);
                            zip.write_all(&buf[..n]).map_err(write_error)?;
                            total += n as u64;
                            done += n as u64;
                            progress(done);
                        }
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                        // A chunk that does not authenticate, a truncated blob: a part of a
                        // file is no copy of it.
                        Err(_) => break false,
                    }
                };
                if complete {
                    entries.push(EntryInfo {
                        name: item.name,
                        size: total,
                        sha256: hex(hasher.finalize().as_slice()),
                    });
                } else {
                    // The writer seeks back to the entry's header: flush, read where the file
                    // is, and cut everything from there, so the partial plaintext is not left
                    // on the disk (H1). A writer that did not move back fails closed.
                    zip.abort_file().map_err(zip_error)?;
                    flush_zip(&mut zip)?;
                    let header = (&file).stream_position().map_err(write_error)?;
                    if header != entry_start {
                        return Err(SaveError::Io(io::Error::other(
                            "the archive writer did not move back",
                        )));
                    }
                    file.set_len(header).map_err(write_error)?;
                    failed.push(item.name);
                }
            }
        }
    }
    let (manifest_name, manifest_bytes) = manifest(&entries, &failed);
    check_entry_name(&manifest_name)?;
    if !seen.insert(manifest_name.clone()) {
        return Err(SaveError::BadName);
    }
    let info = add_bytes(&mut zip, &manifest_name, &manifest_bytes, options)?;
    entries.push(info);
    let mut out = zip.finish().map_err(zip_error)?;
    out.flush().map_err(write_error)?;
    drop(out);
    // (An aborted entry's bytes were cut off at once, so the end is the end of the end record.)
    let end = (&file).stream_position().map_err(write_error)?;
    // The last look at the cancel flag: a cancel that comes after it ends `Done` (L3).
    if options.cancel.load(Ordering::Relaxed) {
        return Err(SaveError::Cancelled);
    }
    sync(file.as_raw_fd())?;
    match part {
        Part::Anonymous => publish_anonymous(&dirfd, &file, &target, options.mode)?,
        Part::Named(hidden) => {
            publish_named(&dirfd, hidden, &target, options.mode, options.staging)?;
        }
    }
    sync_dir(&dirfd)?;
    tracing::info!(
        entries = entries.len(),
        failed = failed.len(),
        bytes = end,
        "an export archive was saved outside the store"
    );
    Ok(ArchiveReport {
        entries,
        failed,
        bytes: end,
    })
}

/// Pushes the writer's buffer into the file, so the file's position is the writer's (called
/// between entries only).
fn flush_zip<W: Write + Seek>(zip: &mut ZipWriter<W>) -> Result<(), SaveError> {
    zip.flush().map_err(write_error)
}

fn add_bytes<W: Write + Seek>(
    zip: &mut ZipWriter<W>,
    name: &str,
    bytes: &[u8],
    options: &ArchiveOptions<'_>,
) -> Result<EntryInfo, SaveError> {
    zip.start_file(
        name,
        entry_options(options, CompressionMethod::Deflated, bytes.len() as u64),
    )
    .map_err(zip_error)?;
    zip.write_all(bytes).map_err(write_error)?;
    Ok(EntryInfo {
        name: name.to_owned(),
        size: bytes.len() as u64,
        sha256: hex(Sha256::digest(bytes).as_slice()),
    })
}

fn entry_options(
    options: &ArchiveOptions<'_>,
    method: CompressionMethod,
    size: u64,
) -> FullFileOptions<'static, 'static> {
    let mut o = SimpleFileOptions::default()
        .compression_method(method)
        .last_modified_time(local_time(options.modified).unwrap_or(zip::DateTime::DEFAULT))
        .unix_permissions(0o644)
        .large_file(size >= options.zip64_from)
        .into_full_options();
    if let Some(secs) = options
        .modified
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| i32::try_from(d.as_secs()).ok())
    {
        let mut field = [1u8; 5];
        field[1..].copy_from_slice(&secs.to_le_bytes());
        // (A failure only leaves the extra field out.)
        let _ = o.add_extra_field(EXTENDED_TIMESTAMP, field, false);
    }
    o
}

/// ZIP's own time field: local time, two-second steps, 1980 to 2107.
fn local_time(time: SystemTime) -> Option<zip::DateTime> {
    let secs = libc::time_t::try_from(time.duration_since(UNIX_EPOCH).ok()?.as_secs()).ok()?;
    // SAFETY: `tm` is plain data for which all-zero is a valid value.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: both pointers are valid for the call; `localtime_r` keeps no state.
    if unsafe { libc::localtime_r(&secs, &mut tm) }.is_null() {
        return None;
    }
    zip::DateTime::from_date_and_time(
        u16::try_from(tm.tm_year.checked_add(1900)?).ok()?,
        u8::try_from(tm.tm_mon.checked_add(1)?).ok()?,
        u8::try_from(tm.tm_mday).ok()?,
        u8::try_from(tm.tm_hour).ok()?,
        u8::try_from(tm.tm_min).ok()?,
        u8::try_from(tm.tm_sec).ok()?,
    )
    .ok()
}

fn write_error(e: io::Error) -> SaveError {
    match e.raw_os_error() {
        Some(code) => classify(code),
        None => SaveError::Io(e),
    }
}

/// (A zip error's text can name an entry: only the file system's own errors are passed on.)
fn zip_error(e: zip::result::ZipError) -> SaveError {
    match e {
        zip::result::ZipError::Io(e) => write_error(e),
        _ => SaveError::Io(io::Error::other("the archive could not be written")),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
