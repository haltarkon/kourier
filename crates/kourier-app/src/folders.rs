//! A folder given to the composer (§8.3): what is inside it, and one archive made of it.
//!
//! A folder cannot travel as a message, so the composer asks what to do with it: add the
//! files inside, or send them as one `.zip`. Both answers start from the same walk:
//!
//! - Every regular file below the folder counts, subfolders too, in a stable order: by path,
//!   each name compared byte for byte, so the order does not depend on the locale or on the
//!   order the file system lists a directory in.
//! - Names that start with a dot are left out, files and folders alike (`.git`, `.env`, a
//!   desktop's `.directory`): what the file manager hides, the user did not see when they
//!   picked the folder. The folder that was given is taken whatever its own name is.
//! - Symbolic links are never followed and never read, whether they point at a file or at a
//!   folder: a link can lead to private files outside the folder the user chose, and a link
//!   to a parent would make the walk endless. The folder that was given may itself be a link
//!   (the user pointed at it). The packing reads no file by its path: it opens the folder
//!   once and each file below that descriptor, one name at a time and never through a link,
//!   so a file or a subfolder that another program swaps for a link after the walk is left
//!   out too. (The list of "add the files inside" holds paths, like any file the user picks:
//!   the core opens them when the send is made.)
//! - Sockets, pipes and devices are left out; a subfolder or a file that cannot be looked at
//!   is left out and counted. What is left out is counted in [`Skipped`], and the UI says so.
//! - A file that goes away while the walk runs is not an error: it is simply not there.
//!
//! [`summarize`] counts (for the question), [`list`] names the files (the answer "add the
//! files inside"), [`pack`] writes the archive (the answer "send as one .zip"). All three
//! block on the disk: they run on a worker thread, never on the UI thread, and stop soon
//! after their [`Cancel`] was used.
//!
//! The archive: a ZIP file with deflate, one entry per file, named `<folder>/<path below it>`
//! with `/` as the separator. An entry name never starts with `/` and never has a `..` part:
//! it is built from the names of the walk, never from a path as given. Every entry keeps its
//! file's modification time (the local time ZIP has always stored, to 2 s, and the exact time
//! in UTC in the "extended timestamp" field that current tools read first) and its permission
//! bits; no owner is stored. A name that is not UTF-8 is written with replacement characters,
//! `\` and control characters become `_` (some extractors take `\` for a separator), and two
//! files whose names come out alike get a number. Archives of 4 GiB and more, files of that
//! size and more than 65,535 entries use ZIP64.
//!
//! Where the archive is written: [`ArchiveFiles`], a private directory in the cache
//! directory, on the disk. Not in the runtime directory, where pasted pictures go: that one
//! is memory (tmpfs), an archive is as large as the chat's file limit (100 MiB on Signal, and
//! several can wait at once), and `/tmp` is memory on many systems too. A pasted picture has
//! never been on a disk, so writing it to one would make a plaintext copy that did not exist;
//! a folder's files already are on a disk, in the clear. The archive is still a second copy,
//! so it is private (directory 0700, file 0600) and short-lived: removed when the composer
//! drops it, once the core has copied it for a send, at exit, and at the next start (a crash
//! leaves it until then). The cache directory is the right home for it for one more reason:
//! backups leave it out.
//!
//! Paths and names are personal data: nothing here logs them, and no error carries one.

use std::collections::HashSet;
use std::ffi::{CString, OsStr, OsString};
use std::fs::{self, File};
use std::io::{self, BufWriter, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rand::RngCore;
use zip::write::{FullFileOptions, SimpleFileOptions};
use zip::{CompressionMethod, ZipWriter};

/// How often a running count or a packing reports how far it is.
pub const PROGRESS_EVERY: Duration = Duration::from_millis(80);

/// A file from this size on is written as a ZIP64 entry. A little below 4 GiB: data that
/// does not compress grows by a few bytes per block, and a file can grow while it is read.
const ZIP64_FROM: u64 = 0xFF00_0000;

/// The "extended timestamp" extra field (Info-ZIP): the modification time in UTC seconds.
const EXTENDED_TIMESTAMP: u16 = 0x5455;

/// The longest archive file name, in bytes (as for a pasted file: well below the 255 a file
/// system takes, with room for a number and the `.zip`).
const NAME_MAX: usize = 200;

/// Stops a walk or a packing from another thread.
#[derive(Clone, Debug, Default)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    /// A handle nobody has used yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Asks the work that holds a clone of this to stop.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    /// Whether [`cancel`](Self::cancel) was called.
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

/// What a walk left out.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Skipped {
    /// Files and folders whose name starts with a dot (a hidden folder counts once, whatever
    /// it holds).
    pub hidden: u64,
    /// Symbolic links, to files or to folders: not followed, not read.
    pub links: u64,
    /// Sockets, pipes and devices.
    pub special: u64,
    /// Files and subfolders that could not be looked at (no permission, an I/O error).
    pub unreadable: u64,
}

impl Skipped {
    /// Everything that was left out.
    pub fn total(&self) -> u64 {
        self.hidden + self.links + self.special + self.unreadable
    }
}

/// What a folder holds, counted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FolderSummary {
    /// Regular files below the folder.
    pub files: u64,
    /// Their sizes added up: also the estimate of the archive's size (deflate makes most
    /// data smaller and nothing much larger).
    pub bytes: u64,
    /// What was left out.
    pub skipped: Skipped,
}

/// One regular file of a walk.
#[derive(Clone, PartialEq, Eq)]
pub struct FolderFile {
    /// The file: the folder's path as it was given, and the names below it.
    pub path: PathBuf,
    /// Its size in bytes when the walk looked at it.
    pub size: u64,
    /// When it was last changed, where the file system says.
    pub modified: Option<SystemTime>,
    /// Its permission bits.
    pub mode: u32,
}

impl std::fmt::Debug for FolderFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FolderFile")
            .field("size", &self.size)
            .finish_non_exhaustive()
    }
}

/// The files of a folder, in walk order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FolderListing {
    /// Every regular file, ordered by path.
    pub files: Vec<FolderFile>,
    /// What was left out.
    pub skipped: Skipped,
}

impl FolderListing {
    /// The sizes of the files added up.
    pub fn bytes(&self) -> u64 {
        self.files
            .iter()
            .fold(0u64, |sum, f| sum.saturating_add(f.size))
    }
}

/// What a finished [`pack`] wrote.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Packed {
    /// Entries in the archive.
    pub files: u64,
    /// Bytes read from the files.
    pub bytes_in: u64,
    /// The size of the archive.
    pub bytes_out: u64,
    /// What the walk left out.
    pub skipped: Skipped,
    /// Files that were there for the walk and could not be read when their turn came (gone,
    /// or an I/O error): they are not in the archive.
    pub missing: u64,
}

/// Why a walk or a packing ended without a result. Log-safe: no path, no name.
#[derive(Debug, thiserror::Error)]
pub enum FolderError {
    /// [`Cancel::cancel`] was called.
    #[error("cancelled")]
    Cancelled,
    /// The path is not a folder (any more).
    #[error("not a folder")]
    NotAFolder,
    /// The folder itself cannot be listed.
    #[error("the folder cannot be read: {0}")]
    Unreadable(io::ErrorKind),
    /// The folder holds no file to pack.
    #[error("no files to pack")]
    Empty,
    /// There is no usable directory for archives.
    #[error("no directory for archives")]
    NoStore,
    /// Writing the archive failed (a full disk, for one).
    #[error("i/o: {0}")]
    Io(io::ErrorKind),
    /// The archive could not be put together.
    #[error("the archive could not be written")]
    Archive,
}

impl From<io::Error> for FolderError {
    fn from(e: io::Error) -> Self {
        // (The kind only: an `io::Error` made by a library may carry a message.)
        Self::Io(e.kind())
    }
}

impl From<zip::result::ZipError> for FolderError {
    fn from(e: zip::result::ZipError) -> Self {
        match e {
            zip::result::ZipError::Io(e) => Self::Io(e.kind()),
            // (Its text can name an entry: not passed on.)
            _ => Self::Archive,
        }
    }
}

/// A name the file manager hides.
fn hidden(name: &OsStr) -> bool {
    name.as_bytes().first() == Some(&b'.')
}

/// The names in `dir`, sorted byte for byte.
fn sorted_names(dir: &Path) -> io::Result<Vec<OsString>> {
    let mut names = Vec::new();
    for entry in fs::read_dir(dir)? {
        names.push(entry?.file_name());
    }
    names.sort_unstable_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
    Ok(names)
}

/// Called with the path of each entry before the walk looks at it.
type Hook<'a> = &'a mut dyn FnMut(&Path);

/// Walks `root` and hands every regular file to `on_file`, in order. `hook` sees each entry
/// first (tests use it to change the folder under the walk).
fn visit(
    root: &Path,
    cancel: &Cancel,
    skipped: &mut Skipped,
    mut hook: Option<Hook<'_>>,
    on_file: &mut dyn FnMut(FolderFile),
) -> Result<(), FolderError> {
    let first = sorted_names(root).map_err(|e| match e.kind() {
        io::ErrorKind::NotADirectory | io::ErrorKind::NotFound => FolderError::NotAFolder,
        kind => FolderError::Unreadable(kind),
    })?;
    // One level per folder being listed: no recursion, however deep the folder is.
    let mut stack: Vec<(PathBuf, std::vec::IntoIter<OsString>)> =
        vec![(root.to_path_buf(), first.into_iter())];
    loop {
        let Some((dir, names)) = stack.last_mut() else {
            return Ok(());
        };
        let Some(name) = names.next() else {
            stack.pop();
            continue;
        };
        if cancel.is_cancelled() {
            return Err(FolderError::Cancelled);
        }
        if hidden(&name) {
            skipped.hidden += 1;
            continue;
        }
        let path = dir.join(&name);
        if let Some(hook) = hook.as_mut() {
            hook(&path);
        }
        // (`symlink_metadata`: a link is seen as a link, never as what it points at.)
        let meta = match fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            // It went away since the folder was listed.
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(_) => {
                skipped.unreadable += 1;
                continue;
            }
        };
        let kind = meta.file_type();
        if kind.is_symlink() {
            skipped.links += 1;
        } else if kind.is_dir() {
            match sorted_names(&path) {
                Ok(names) => stack.push((path, names.into_iter())),
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(_) => skipped.unreadable += 1,
            }
        } else if kind.is_file() {
            on_file(FolderFile {
                path,
                size: meta.len(),
                modified: meta.modified().ok(),
                mode: meta.permissions().mode(),
            });
        } else {
            skipped.special += 1;
        }
    }
}

/// Counts what `root` holds. `tick` gets the count so far about every [`PROGRESS_EVERY`]
/// (a large folder on a slow disk takes a while). Blocking.
pub fn summarize(
    root: &Path,
    cancel: &Cancel,
    tick: &mut dyn FnMut(&FolderSummary),
) -> Result<FolderSummary, FolderError> {
    let mut skipped = Skipped::default();
    let (mut files, mut bytes) = (0u64, 0u64);
    let mut last = Instant::now();
    visit(root, cancel, &mut skipped, None, &mut |file| {
        files += 1;
        bytes = bytes.saturating_add(file.size);
        // (The clock is read once per 64 files: a walk is mostly `stat` calls.)
        if files % 64 == 0 && last.elapsed() >= PROGRESS_EVERY {
            last = Instant::now();
            tick(&FolderSummary {
                files,
                bytes,
                skipped: Skipped::default(),
            });
        }
    })?;
    Ok(FolderSummary {
        files,
        bytes,
        skipped,
    })
}

/// Names the files `root` holds, in order. Blocking.
pub fn list(root: &Path, cancel: &Cancel) -> Result<FolderListing, FolderError> {
    list_with(root, cancel, None)
}

fn list_with(
    root: &Path,
    cancel: &Cancel,
    hook: Option<Hook<'_>>,
) -> Result<FolderListing, FolderError> {
    let mut listing = FolderListing::default();
    let FolderListing { files, skipped } = &mut listing;
    visit(root, cancel, skipped, hook, &mut |file| files.push(file))?;
    Ok(listing)
}

/// One part of an entry name: UTF-8, no separator of any system, no control character.
fn entry_part(name: &OsStr) -> String {
    let part: String = name
        .to_string_lossy()
        .chars()
        .map(|c| match c {
            '/' | '\\' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    match part.as_str() {
        "" | "." | ".." => "_".to_owned(),
        _ => part,
    }
}

/// `name` with ` (n)` before its extension.
fn numbered(name: &str, n: u32) -> String {
    let (dir, file) = match name.rsplit_once('/') {
        Some((dir, file)) => (Some(dir), file),
        None => (None, name),
    };
    let file = match file.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => format!("{stem} ({n}).{ext}"),
        _ => format!("{file} ({n})"),
    };
    match dir {
        Some(dir) => format!("{dir}/{file}"),
        None => file,
    }
}

/// The entry name of `file`: `top`, then its path below `root`, joined with `/`. Built from
/// plain names only, so it is relative and has no `..` part; `None` for a path that is not
/// below `root` (the walk makes no such path).
fn entry_name(top: &str, root: &Path, file: &Path, used: &mut HashSet<String>) -> Option<String> {
    let below = file.strip_prefix(root).ok()?;
    let mut name = String::from(top);
    let mut parts = 0;
    for part in below.components() {
        let std::path::Component::Normal(part) = part else {
            return None;
        };
        name.push('/');
        name.push_str(&entry_part(part));
        parts += 1;
    }
    if parts == 0 {
        return None;
    }
    // Two files can come out alike (names that were not UTF-8, a `\` beside a `_`): an
    // archive holds each name once.
    let mut unique = name.clone();
    let mut n = 1;
    while !used.insert(unique.clone()) {
        n += 1;
        unique = numbered(&name, n);
    }
    Some(unique)
}

/// The time as ZIP's own field stores it: local time, to two seconds, from 1980 to 2107.
fn local_time(time: SystemTime) -> Option<zip::DateTime> {
    let secs = time.duration_since(UNIX_EPOCH).ok()?.as_secs();
    let secs = libc::time_t::try_from(secs).ok()?;
    // SAFETY: `tm` is plain data for which all-zero is a valid value.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: both pointers are valid for the call, and `localtime_r` keeps no state of its
    // own between calls (it is the thread-safe form).
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

/// How one file is stored: deflate, its times, its permission bits.
fn entry_options(file: &FolderFile) -> Result<FullFileOptions<'static>, FolderError> {
    let mut options = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        // (Without a usable time: 1980-01-01, never the time of packing.)
        .last_modified_time(
            file.modified
                .and_then(local_time)
                .unwrap_or(zip::DateTime::DEFAULT),
        )
        .unix_permissions(file.mode & 0o777)
        .large_file(file.size >= ZIP64_FROM)
        .into_full_options();
    let utc = file
        .modified
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .and_then(|d| u32::try_from(d.as_secs()).ok());
    if let Some(secs) = utc {
        // Flags (bit 0: the modification time follows), then the time, little-endian.
        let mut field = [1u8; 5];
        field[1..].copy_from_slice(&secs.to_le_bytes());
        options.add_extra_data(EXTENDED_TIMESTAMP, field, false)?;
    }
    Ok(options)
}

/// `openat` with `O_NOFOLLOW` and `O_CLOEXEC`: `name` in the directory `dir`, never through a
/// symbolic link.
fn open_at(dir: &impl AsRawFd, name: &OsStr, flags: libc::c_int) -> io::Result<OwnedFd> {
    let name = CString::new(name.as_bytes()).map_err(|_| io::ErrorKind::InvalidInput)?;
    loop {
        // SAFETY: `name` is a NUL-terminated string that lives through the call, and `dir`
        // is an open descriptor for as long as it is borrowed.
        let fd = unsafe {
            libc::openat(
                dir.as_raw_fd(),
                name.as_ptr(),
                flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd >= 0 {
            // SAFETY: the kernel just handed this descriptor out and nothing else owns it.
            return Ok(unsafe { OwnedFd::from_raw_fd(fd) });
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

/// Opens the regular file at `below` (plain names only) under the open folder `top` for
/// reading, one step at a time, each without following a symbolic link. A path string is
/// resolved anew by every call that takes it, so a subfolder that was replaced by a link
/// after the walk would lead an `open` of the whole path out of the folder, whatever
/// `O_NOFOLLOW` says about the last name. Going down descriptor by descriptor, nothing
/// outside the folder that `top` is can be reached. `None`: not there (any more), a link on
/// the way, or no regular file.
fn open_below(top: &File, below: &Path) -> Option<File> {
    let mut parts = below.components().peekable();
    let mut dir: Option<OwnedFd> = None;
    while let Some(part) = parts.next() {
        let std::path::Component::Normal(name) = part else {
            return None;
        };
        let last = parts.peek().is_none();
        let flags = if last {
            // (`O_NONBLOCK`: a pipe that took the file's place does not hold the open up.)
            libc::O_RDONLY | libc::O_NONBLOCK
        } else {
            libc::O_RDONLY | libc::O_DIRECTORY
        };
        let next = match &dir {
            Some(dir) => open_at(dir, name, flags),
            None => open_at(top, name, flags),
        }
        .ok()?;
        if last {
            let file = File::from(next);
            return file.metadata().is_ok_and(|m| m.is_file()).then_some(file);
        }
        dir = Some(next);
    }
    None
}

/// Packs the files of `root` into a new archive at `dest` (which must not exist; it is made
/// with mode 0600), under the top-level name `top`. `progress(done, total)` reports the bytes
/// read so far about every [`PROGRESS_EVERY`], and once at the end. Blocking.
///
/// On any error, and when cancelled, nothing is left at `dest`.
pub fn pack(
    root: &Path,
    top: &str,
    dest: &Path,
    cancel: &Cancel,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<Packed, FolderError> {
    let listing = list(root, cancel)?;
    if listing.files.is_empty() {
        return Err(FolderError::Empty);
    }
    let file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(dest)?;
    let written = write_archive(root, top, &listing, file, cancel, progress);
    if written.is_err() {
        // (Best effort: the store wipes its directory at exit and at the next start too.)
        let _ = fs::remove_file(dest);
    }
    written
}

fn write_archive(
    root: &Path,
    top: &str,
    listing: &FolderListing,
    file: File,
    cancel: &Cancel,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<Packed, FolderError> {
    let total = listing.bytes();
    let top = entry_part(OsStr::new(top));
    // The folder, opened once (through a link when the user pointed at one): every file is
    // then opened below this descriptor ([`open_below`]), not by its path.
    let folder = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY)
        .open(root)
        .map_err(|e| match e.kind() {
            io::ErrorKind::NotADirectory | io::ErrorKind::NotFound => FolderError::NotAFolder,
            kind => FolderError::Unreadable(kind),
        })?;
    let mut zip = ZipWriter::new(BufWriter::new(file));
    let mut used = HashSet::new();
    let mut packed = Packed {
        skipped: listing.skipped,
        ..Packed::default()
    };
    // Bytes of the files dealt with so far, as the walk measured them: what `progress`
    // counts against `total`, whatever a file's size is by now.
    let mut before = 0u64;
    let mut last = Instant::now();
    let mut buf = vec![0u8; 64 * 1024];
    for entry in &listing.files {
        if cancel.is_cancelled() {
            return Err(FolderError::Cancelled);
        }
        let Some(name) = entry_name(&top, root, &entry.path, &mut used) else {
            packed.missing += 1;
            before = before.saturating_add(entry.size);
            continue;
        };
        // (A file, or a folder on the way to it, that was replaced by a link since the walk
        // is not read.)
        let source = entry
            .path
            .strip_prefix(root)
            .ok()
            .and_then(|below| open_below(&folder, below));
        let Some(mut source) = source else {
            packed.missing += 1;
            before = before.saturating_add(entry.size);
            continue;
        };
        zip.start_file(name, entry_options(entry)?)?;
        let mut read = 0u64;
        let complete = loop {
            if cancel.is_cancelled() {
                return Err(FolderError::Cancelled);
            }
            match source.read(&mut buf) {
                Ok(0) => break true,
                Ok(n) => {
                    zip.write_all(&buf[..n])?;
                    read += n as u64;
                    if last.elapsed() >= PROGRESS_EVERY {
                        last = Instant::now();
                        progress(
                            before.saturating_add(read.min(entry.size)).min(total),
                            total,
                        );
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                // The file cannot be read to its end: a part of it is no copy of it.
                Err(_) => break false,
            }
        };
        if complete {
            packed.files += 1;
            packed.bytes_in += read;
        } else {
            zip.abort_file()?;
            packed.missing += 1;
        }
        before = before.saturating_add(entry.size);
    }
    if packed.files == 0 {
        return Err(FolderError::Empty);
    }
    let mut out = zip.finish()?;
    out.flush()?;
    let file = out
        .into_inner()
        .map_err(|e| FolderError::from(e.into_error()))?;
    packed.bytes_out = file.metadata()?.len();
    progress(total, total);
    Ok(packed)
}

/// The file name of the archive of a folder called `folder`: `<folder>.zip`, as a plain file
/// name of at most [`NAME_MAX`] bytes.
pub fn archive_name(folder: &str) -> String {
    let mut stem = entry_part(OsStr::new(folder));
    let room = NAME_MAX - ".zip".len();
    if stem.len() > room {
        let mut end = room;
        while !stem.is_char_boundary(end) {
            end -= 1;
        }
        stem.truncate(end);
    }
    format!("{stem}.zip")
}

/// The archives made for the composer: a private directory on the disk (see the module
/// documentation for why it is not the tmpfs the pasted pictures use).
pub struct ArchiveFiles {
    dir: Option<PathBuf>,
    /// Directories made by [`reserve`](Self::reserve) (one per archive, so each keeps its
    /// name). Never held while the disk is touched: the UI thread takes this lock
    /// ([`owns`](Self::owns), [`take`](Self::take)).
    dirs: Mutex<Vec<PathBuf>>,
    /// Held while a place is made on the disk and while everything is wiped, so that the two
    /// never overlap; only worker threads and the exit take it. Its value:
    /// [`clear`](Self::clear) ran, the app is closing and no place is handed out any more.
    gate: Mutex<bool>,
}

impl std::fmt::Debug for ArchiveFiles {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ArchiveFiles")
            .field("enabled", &self.dir.is_some())
            .field("archives", &self.lock().len())
            .finish()
    }
}

impl ArchiveFiles {
    /// Archives below `dir`, which is made private (0700) and emptied now: what a run that
    /// crashed left behind goes. Without a usable directory, packing is refused.
    pub fn new(dir: &Path) -> Self {
        let ready = crate::paths::create_private_dir(dir).and_then(|()| wipe(dir));
        if let Err(e) = &ready {
            tracing::warn!(error = %e, "no directory for archives; folders cannot be packed");
        }
        Self {
            dir: ready.is_ok().then(|| dir.to_path_buf()),
            dirs: Mutex::new(Vec::new()),
            gate: Mutex::new(false),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<PathBuf>> {
        self.dirs.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Where the archive of a folder called `folder` is to be written: a path in a new
    /// private directory of its own. The file does not exist yet ([`pack`] makes it).
    pub fn reserve(&self, folder: &str) -> Result<PathBuf, FolderError> {
        let base = self.dir.as_ref().ok_or(FolderError::NoStore)?;
        // (Checked and made under the gate `clear` takes: a place is handed out before the
        // wipe, or not at all. The list's own lock is taken only to add the place: making a
        // directory can wait for a busy disk, and the UI thread must not wait with it.)
        let closed = self.gate.lock().unwrap_or_else(PoisonError::into_inner);
        if *closed {
            return Err(FolderError::NoStore);
        }
        let mut random = [0u8; 12];
        rand::rng().fill_bytes(&mut random);
        let dir = base.join(
            random
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>(),
        );
        fs::DirBuilder::new().mode(0o700).create(&dir)?;
        self.lock().push(dir.clone());
        Ok(dir.join(archive_name(folder)))
    }

    /// Whether `path` is a path [`reserve`](Self::reserve) handed out.
    pub fn owns(&self, path: &Path) -> bool {
        path.parent()
            .is_some_and(|parent| self.lock().iter().any(|d| d == parent))
    }

    /// Forgets the archive at `path` (or the place reserved for one) and returns the
    /// directory to delete with [`remove_place`]; `None` for a path that is not ours. Does
    /// nothing on the disk itself: an archive can be large and the disk slow, so the caller
    /// chooses the thread that deletes (never the UI's).
    pub fn take(&self, path: &Path) -> Option<PathBuf> {
        let parent = path.parent()?;
        let mut dirs = self.lock();
        let at = dirs.iter().position(|d| d == parent)?;
        Some(dirs.swap_remove(at))
    }

    /// Deletes an archive (or the place reserved for one) now; other paths are left alone.
    /// Returns whether it was one of ours. Blocking.
    pub fn release(&self, path: &Path) -> bool {
        match self.take(path) {
            Some(dir) => {
                remove_place(&dir);
                true
            }
            None => false,
        }
    }

    /// Deletes every archive and hands out no more places (exit): a packing that is still
    /// running on a worker thread when the app closes finds nowhere to write its next
    /// archive, so nothing new is left behind after the wipe.
    pub fn clear(&self) {
        let mut closed = self.gate.lock().unwrap_or_else(PoisonError::into_inner);
        *closed = true;
        self.lock().clear();
        if let Some(dir) = &self.dir
            && let Err(e) = wipe(dir)
            && e.kind() != io::ErrorKind::NotFound
        {
            tracing::warn!(error = %e, "wiping the archive directory failed");
        }
    }
}

impl Drop for ArchiveFiles {
    fn drop(&mut self) {
        self.clear();
    }
}

/// Deletes the directory of one archive ([`ArchiveFiles::take`]). Blocking.
pub fn remove_place(dir: &Path) {
    if let Err(e) = fs::remove_dir_all(dir)
        && e.kind() != io::ErrorKind::NotFound
    {
        tracing::debug!(error = %e, "removing an archive failed");
    }
}

/// Removes everything inside `dir` (not `dir` itself).
fn wipe(dir: &Path) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        // (Never through a link: only what lies in this directory goes.)
        let removed = match fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_dir() => fs::remove_dir_all(&path),
            Ok(_) => fs::remove_file(&path),
            Err(e) => Err(e),
        };
        match removed {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::fs::symlink;

    fn write(path: &Path, bytes: &[u8]) {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).unwrap();
        }
        fs::write(path, bytes).unwrap();
    }

    /// The paths of `listing` below `root`, as text.
    fn below(root: &Path, listing: &FolderListing) -> Vec<String> {
        listing
            .files
            .iter()
            .map(|f| {
                f.path
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect()
    }

    /// A folder with everything the walk has rules for.
    fn sample(dir: &Path) -> PathBuf {
        let root = dir.join("Survey");
        write(&root.join("notes.txt"), b"wind 4 m/s");
        write(&root.join("B.txt"), b"capital first");
        write(&root.join("raw/day 1/a.bin"), &[7u8; 5000]);
        write(&root.join("raw/day 1/b.bin"), &[]);
        write(&root.join("raw/z.csv"), b"t;lat;lon\n");
        write(&root.join("raw/day 1/deeper/still/here.txt"), b"deep");
        // Hidden: a file, and a folder with files of its own.
        write(&root.join(".env"), b"SECRET=1");
        write(&root.join(".git/config"), b"[core]");
        write(&root.join("raw/.directory"), b"[Desktop Entry]");
        // Links: to a file outside, to a folder outside, to the parent, and a dangling one.
        let outside = dir.join("private");
        write(&outside.join("diary.txt"), b"not for anyone");
        symlink(outside.join("diary.txt"), root.join("link to file")).unwrap();
        symlink(&outside, root.join("link to folder")).unwrap();
        symlink(&root, root.join("raw/up")).unwrap();
        symlink(dir.join("nowhere"), root.join("dangling")).unwrap();
        fs::create_dir_all(root.join("empty")).unwrap();
        root
    }

    #[test]
    fn the_walk_takes_every_file_in_order_and_leaves_out_hidden_ones_and_links() {
        let dir = tempfile::tempdir().unwrap();
        let root = sample(dir.path());
        let listing = list(&root, &Cancel::new()).unwrap();
        // By path, byte for byte: capitals before small letters, a folder among the files.
        assert_eq!(
            below(&root, &listing),
            [
                "B.txt",
                "notes.txt",
                "raw/day 1/a.bin",
                "raw/day 1/b.bin",
                "raw/day 1/deeper/still/here.txt",
                "raw/z.csv",
            ]
        );
        assert_eq!(
            listing.skipped,
            Skipped {
                // `.env`, `.git` (once, whatever it holds) and `raw/.directory`.
                hidden: 3,
                links: 4,
                special: 0,
                unreadable: 0
            }
        );
        assert_eq!(listing.bytes(), 10 + 13 + 5000 + 10 + 4);
        let a = &listing.files[2];
        assert_eq!((a.size, a.mode & 0o777 != 0), (5000, true));
        assert!(a.modified.is_some());
        // The count for the question is the same walk.
        let mut ticks = 0;
        let summary = summarize(&root, &Cancel::new(), &mut |_| ticks += 1).unwrap();
        assert_eq!(
            summary,
            FolderSummary {
                files: 6,
                bytes: listing.bytes(),
                skipped: listing.skipped
            }
        );
        // Nothing outside the folder was read: the same walk twice gives the same list.
        assert_eq!(list(&root, &Cancel::new()).unwrap(), listing);
        assert!(
            format!("{listing:?}").find("notes").is_none(),
            "no names in Debug"
        );
    }

    #[test]
    fn the_folder_that_was_given_is_taken_whatever_its_name_and_through_a_link() {
        let dir = tempfile::tempdir().unwrap();
        // A hidden folder the user picked on purpose.
        let hidden_root = dir.path().join(".config");
        write(&hidden_root.join("app.toml"), b"x = 1");
        write(&hidden_root.join(".cache"), b"skip me");
        let listing = list(&hidden_root, &Cancel::new()).unwrap();
        assert_eq!(below(&hidden_root, &listing), ["app.toml"]);
        assert_eq!(listing.skipped.hidden, 1);
        // A link the user pointed at: the folder behind it is walked, links inside are not.
        let link = dir.path().join("shortcut");
        symlink(&hidden_root, &link).unwrap();
        let listing = list(&link, &Cancel::new()).unwrap();
        assert_eq!(below(&link, &listing), ["app.toml"]);
        // No folder: a file, or nothing at all.
        assert!(matches!(
            list(&hidden_root.join("app.toml"), &Cancel::new()),
            Err(FolderError::NotAFolder)
        ));
        assert!(matches!(
            list(&dir.path().join("gone"), &Cancel::new()),
            Err(FolderError::NotAFolder)
        ));
    }

    #[test]
    fn names_that_are_not_utf8_are_walked_and_packed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("Old disk");
        fs::create_dir_all(&root).unwrap();
        // Latin-1 bytes: "caf\xe9.txt" and "caf\xe8.txt" both read "caf\u{fffd}.txt".
        let e_acute = OsString::from_vec(b"caf\xe9.txt".to_vec());
        let e_grave = OsString::from_vec(b"caf\xe8.txt".to_vec());
        let sub = OsString::from_vec(b"dir\xff".to_vec());
        write(&root.join(&e_acute), b"acute");
        write(&root.join(&e_grave), b"grave");
        write(&root.join(&sub).join("in.txt"), b"inside");
        write(&root.join("back\\slash.txt"), b"bs");
        write(&root.join("back_slash.txt"), b"us");
        let listing = list(&root, &Cancel::new()).unwrap();
        assert_eq!(listing.files.len(), 5);
        // The paths are the real ones, byte for byte (0xe8 sorts before 0xe9).
        assert_eq!(listing.files[2].path, root.join(&e_grave));
        assert_eq!(listing.files[3].path, root.join(&e_acute));
        assert_eq!(listing.files[4].path, root.join(&sub).join("in.txt"));

        let dest = dir.path().join("out.zip");
        let packed = pack(&root, "Old disk", &dest, &Cancel::new(), &mut |_, _| {}).unwrap();
        assert_eq!((packed.files, packed.missing), (5, 0));
        let mut archive = zip::ZipArchive::new(File::open(&dest).unwrap()).unwrap();
        let mut names: Vec<String> = archive.file_names().map(str::to_owned).collect();
        names.sort();
        assert_eq!(
            names,
            [
                "Old disk/back_slash (2).txt",
                "Old disk/back_slash.txt",
                "Old disk/caf\u{fffd} (2).txt",
                "Old disk/caf\u{fffd}.txt",
                "Old disk/dir\u{fffd}/in.txt",
            ]
        );
        // Each name holds its own file: the two that came out alike are both there.
        let mut text = String::new();
        archive
            .by_name("Old disk/caf\u{fffd}.txt")
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();
        assert_eq!(text, "grave");
        text.clear();
        archive
            .by_name("Old disk/caf\u{fffd} (2).txt")
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();
        assert_eq!(text, "acute");
        text.clear();
        archive
            .by_name("Old disk/back_slash.txt")
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();
        assert_eq!(text, "bs", "the name with the backslash came first");
    }

    #[test]
    fn a_file_that_vanishes_while_walking_is_simply_not_there() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("Busy");
        write(&root.join("a.txt"), b"a");
        write(&root.join("b.txt"), b"b");
        write(&root.join("c/d.txt"), b"d");
        write(&root.join("e.txt"), b"e");
        // The folder was listed with `b.txt` and `c` in it; both go before the walk gets to
        // them (another program is deleting files).
        let (b, c) = (root.join("b.txt"), root.join("c"));
        let mut hook = |path: &Path| {
            if path == b {
                fs::remove_file(&b).unwrap();
                fs::remove_dir_all(&c).unwrap();
            }
        };
        let listing = list_with(&root, &Cancel::new(), Some(&mut hook)).unwrap();
        assert_eq!(below(&root, &listing), ["a.txt", "e.txt"]);
        assert_eq!(listing.skipped, Skipped::default(), "gone is not an error");

        // The same between the walk and the packing: the archive holds what could be read
        // and says how many files it lacks.
        write(&root.join("b.txt"), b"b again");
        let dest = dir.path().join("busy.zip");
        let gone = root.join("e.txt");
        let listing = list(&root, &Cancel::new()).unwrap();
        fs::remove_file(&gone).unwrap();
        let file = File::create(&dest).unwrap();
        let packed = write_archive(
            &root,
            "Busy",
            &listing,
            file,
            &Cancel::new(),
            &mut |_, _| {},
        )
        .unwrap();
        assert_eq!((packed.files, packed.missing), (2, 1));
        let archive = zip::ZipArchive::new(File::open(&dest).unwrap()).unwrap();
        let mut names: Vec<&str> = archive.file_names().collect();
        names.sort_unstable();
        assert_eq!(names, ["Busy/a.txt", "Busy/b.txt"]);
    }

    #[test]
    fn a_file_replaced_by_a_link_after_the_walk_is_not_read() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("Shared");
        write(&root.join("report.txt"), b"public");
        write(&dir.path().join("secret.txt"), b"private");
        let listing = list(&root, &Cancel::new()).unwrap();
        fs::remove_file(root.join("report.txt")).unwrap();
        symlink(dir.path().join("secret.txt"), root.join("report.txt")).unwrap();
        let dest = dir.path().join("shared.zip");
        let file = File::create(&dest).unwrap();
        let result = write_archive(
            &root,
            "Shared",
            &listing,
            file,
            &Cancel::new(),
            &mut |_, _| {},
        );
        assert!(
            matches!(result, Err(FolderError::Empty)),
            "the link was not followed: {result:?}"
        );
    }

    #[test]
    fn a_subfolder_replaced_by_a_link_after_the_walk_leads_nowhere() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("Shared");
        write(&root.join("top.txt"), b"top");
        write(&root.join("sub/report.txt"), b"public");
        write(&root.join("sub/deeper/notes.txt"), b"public too");
        // The same names outside the folder, with other content.
        let outside = dir.path().join("outside");
        write(&outside.join("report.txt"), b"PRIVATE-REPORT");
        write(&outside.join("deeper/notes.txt"), b"PRIVATE-NOTES");
        let listing = list(&root, &Cancel::new()).unwrap();
        assert_eq!(listing.files.len(), 3);
        // Another program swaps the subfolder for a link between the walk and the packing:
        // by their paths, the two files below it now are the ones outside.
        fs::rename(root.join("sub"), dir.path().join("moved away")).unwrap();
        symlink(&outside, root.join("sub")).unwrap();
        assert_eq!(
            fs::read(&listing.files[0].path).unwrap(),
            b"PRIVATE-NOTES",
            "the path leads out of the folder now"
        );
        let dest = dir.path().join("shared.zip");
        let file = File::create(&dest).unwrap();
        let packed = write_archive(
            &root,
            "Shared",
            &listing,
            file,
            &Cancel::new(),
            &mut |_, _| {},
        )
        .unwrap();
        assert_eq!((packed.files, packed.missing), (1, 2));
        let bytes = fs::read(&dest).unwrap();
        let archive = zip::ZipArchive::new(Cursor::new(&bytes)).unwrap();
        assert_eq!(archive.file_names().collect::<Vec<_>>(), ["Shared/top.txt"]);
        assert!(!bytes.windows(7).any(|w| w == b"PRIVATE"));

        // The folder that was given may be a link itself; what lies below it is still
        // opened step by step.
        let shortcut = dir.path().join("shortcut");
        symlink(&root, &shortcut).unwrap();
        let listing = list(&shortcut, &Cancel::new()).unwrap();
        assert_eq!(listing.files.len(), 1, "the link inside is not walked");
        fs::remove_file(root.join("sub")).unwrap();
        fs::rename(dir.path().join("moved away"), root.join("sub")).unwrap();
        let listing = list(&shortcut, &Cancel::new()).unwrap();
        let again = dir.path().join("again.zip");
        let packed = pack(&shortcut, "Shared", &again, &Cancel::new(), &mut |_, _| {}).unwrap();
        assert_eq!((packed.files, packed.missing), (3, 0));
        assert_eq!(listing.files.len(), 3);
        // A name that is no plain name never leaves the folder either.
        let top = File::open(&root).unwrap();
        assert!(open_below(&top, Path::new("top.txt")).is_some());
        assert!(open_below(&top, Path::new("sub/deeper/notes.txt")).is_some());
        assert!(open_below(&top, Path::new("../outside/report.txt")).is_none());
        assert!(open_below(&top, Path::new("/etc/hostname")).is_none());
        assert!(
            open_below(&top, Path::new("sub")).is_none(),
            "a folder is no file"
        );
        assert!(open_below(&top, Path::new("")).is_none());
    }

    #[test]
    fn an_unreadable_subfolder_is_counted_and_an_unreadable_folder_is_an_error() {
        // (Root reads everything: nothing to test then.)
        // SAFETY: `geteuid` only reads the process's credentials.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("Mixed");
        write(&root.join("ok.txt"), b"ok");
        write(&root.join("locked/in.txt"), b"in");
        let locked = root.join("locked");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        let listing = list(&root, &Cancel::new());
        let whole = list(&locked, &Cancel::new());
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).unwrap();
        let listing = listing.unwrap();
        assert_eq!(below(&root, &listing), ["ok.txt"]);
        assert_eq!(listing.skipped.unreadable, 1);
        assert!(matches!(
            whole,
            Err(FolderError::Unreadable(io::ErrorKind::PermissionDenied))
        ));

        // A folder whose only file cannot be opened: the walk counts the file (it can be
        // looked at), the packing cannot read it, and the archive that was begun is removed.
        let sealed = dir.path().join("Sealed");
        write(&sealed.join("x.bin"), b"sealed");
        fs::set_permissions(sealed.join("x.bin"), fs::Permissions::from_mode(0o000)).unwrap();
        assert_eq!(list(&sealed, &Cancel::new()).unwrap().files.len(), 1);
        let dest = dir.path().join("sealed.zip");
        let result = pack(&sealed, "Sealed", &dest, &Cancel::new(), &mut |_, _| {});
        fs::set_permissions(sealed.join("x.bin"), fs::Permissions::from_mode(0o600)).unwrap();
        assert!(matches!(result, Err(FolderError::Empty)), "{result:?}");
        assert!(!dest.exists(), "nothing is left of an archive that failed");
    }

    #[test]
    fn a_walk_and_a_packing_stop_when_cancelled_and_leave_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("Large");
        for n in 0..40 {
            write(&root.join(format!("{n:02}.bin")), &vec![n as u8; 70_000]);
        }
        // Cancelled before it starts.
        let cancel = Cancel::new();
        cancel.cancel();
        assert!(matches!(list(&root, &cancel), Err(FolderError::Cancelled)));
        assert!(matches!(
            summarize(&root, &cancel, &mut |_| {}),
            Err(FolderError::Cancelled)
        ));
        // Cancelled part way through the walk.
        let cancel = Cancel::new();
        let mut seen = 0;
        let stop = cancel.clone();
        let mut hook = |_: &Path| {
            seen += 1;
            if seen == 10 {
                stop.cancel();
            }
        };
        assert!(matches!(
            list_with(&root, &cancel, Some(&mut hook)),
            Err(FolderError::Cancelled)
        ));
        assert_eq!(seen, 10, "it stops at the next entry");
        // Cancelled while packing (from the progress report, as the UI's button would from
        // another thread): the error is "cancelled" and no file is left.
        let store = ArchiveFiles::new(&dir.path().join("cache/outgoing"));
        let dest = store.reserve("Large").unwrap();
        let cancel = Cancel::new();
        let stop = cancel.clone();
        let listing = list(&root, &cancel).unwrap();
        let file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&dest)
            .unwrap();
        // (The reports are paced by the clock: the walk's own count stops it instead, at the
        // eleventh file, as a button pressed at that moment would.)
        let mut entries = 0;
        let result = {
            let mut used = HashSet::new();
            let mut zip = ZipWriter::new(BufWriter::new(file));
            let mut stopped = None;
            for f in &listing.files {
                if cancel.is_cancelled() {
                    stopped = Some(FolderError::Cancelled);
                    break;
                }
                let name = entry_name("Large", &root, &f.path, &mut used).unwrap();
                zip.start_file(name, entry_options(f).unwrap()).unwrap();
                zip.write_all(&fs::read(&f.path).unwrap()).unwrap();
                entries += 1;
                if entries == 11 {
                    stop.cancel();
                }
            }
            stopped
        };
        assert!(matches!(result, Some(FolderError::Cancelled)));
        assert_eq!(entries, 11);
        fs::remove_file(&dest).unwrap();
        // The real thing, cancelled from its first report, as the UI does it. A report needs
        // the clock to move on, so there is something to pack that takes a while: data
        // that does not compress. (On a machine so fast that no report comes before the
        // end, the archive is simply written.)
        let mut noise = vec![0u8; 24 * 1024 * 1024];
        let mut x = 0x2545_f491_4f6c_dd1du64;
        for byte in &mut noise {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            *byte = (x >> 24) as u8;
        }
        write(&root.join("noise.bin"), &noise);
        let cancel = Cancel::new();
        let stop = cancel.clone();
        let mut reports = 0;
        let result = pack(&root, "Large", &dest, &cancel, &mut |done, total| {
            reports += 1;
            if done < total {
                stop.cancel();
            }
        });
        if cancel.is_cancelled() {
            assert!(matches!(result, Err(FolderError::Cancelled)), "{result:?}");
            assert!(!dest.exists(), "a cancelled archive is removed");
        } else {
            assert!(result.is_ok() && reports == 1);
            fs::remove_file(&dest).unwrap();
        }
        // Cancelled before the first byte: nothing was created at all.
        let cancel = Cancel::new();
        cancel.cancel();
        let dest = store.reserve("Large").unwrap();
        assert!(matches!(
            pack(&root, "Large", &dest, &cancel, &mut |_, _| {}),
            Err(FolderError::Cancelled)
        ));
        assert!(!dest.exists());
    }

    #[test]
    fn the_archive_holds_every_file_with_its_bytes_its_path_and_its_time() {
        let dir = tempfile::tempdir().unwrap();
        let root = sample(dir.path());
        // A time of its own, on an odd second (ZIP's own field keeps even seconds only).
        let when = UNIX_EPOCH + Duration::from_secs(1_700_000_001);
        let notes = File::options()
            .write(true)
            .open(root.join("notes.txt"))
            .unwrap();
        notes.set_modified(when).unwrap();
        fs::set_permissions(root.join("B.txt"), fs::Permissions::from_mode(0o640)).unwrap();

        let store = ArchiveFiles::new(&dir.path().join("cache/outgoing"));
        let dest = store.reserve("Survey").unwrap();
        assert_eq!(dest.file_name().unwrap(), "Survey.zip");
        let mut reports: Vec<(u64, u64)> = Vec::new();
        let packed = pack(
            &root,
            "Survey",
            &dest,
            &Cancel::new(),
            &mut |done, total| {
                reports.push((done, total));
            },
        )
        .unwrap();
        let total = 10 + 13 + 5000 + 10 + 4;
        assert_eq!(
            packed,
            Packed {
                files: 6,
                bytes_in: total,
                bytes_out: fs::metadata(&dest).unwrap().len(),
                skipped: Skipped {
                    hidden: 3,
                    links: 4,
                    special: 0,
                    unreadable: 0
                },
                missing: 0
            }
        );
        assert_eq!(reports.last(), Some(&(total, total)), "the end is reported");
        assert!(reports.windows(2).all(|w| w[0].0 <= w[1].0), "never back");
        // Private: 0600 in a 0700 directory of its own.
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dest), 0o600);
        assert_eq!(mode(dest.parent().unwrap()), 0o700);

        // Read back: the entries, in walk order, each with the bytes of its file.
        let bytes = fs::read(&dest).unwrap();
        let mut archive = zip::ZipArchive::new(Cursor::new(&bytes)).unwrap();
        let names: Vec<String> = (0..archive.len())
            .map(|i| archive.by_index(i).unwrap().name().to_owned())
            .collect();
        assert_eq!(
            names,
            [
                "Survey/B.txt",
                "Survey/notes.txt",
                "Survey/raw/day 1/a.bin",
                "Survey/raw/day 1/b.bin",
                "Survey/raw/day 1/deeper/still/here.txt",
                "Survey/raw/z.csv",
            ]
        );
        for name in &names {
            // Relative, no `..`, nothing hidden, nothing that was behind a link.
            assert!(!name.starts_with('/') && !name.contains("..") && !name.contains("/."));
            assert!(!name.contains("link") && !name.contains("diary") && !name.contains("up/"));
            let mut entry = archive.by_name(name).unwrap();
            let mut content = Vec::new();
            entry.read_to_end(&mut content).unwrap();
            let below = name.strip_prefix("Survey/").unwrap();
            assert_eq!(content, fs::read(root.join(below)).unwrap(), "{name}");
            assert!(entry.is_file());
            assert!(
                entry.enclosed_name().is_some(),
                "{name} stays inside on extraction"
            );
        }
        // Nothing of the hidden files or of the files behind the links is in the archive.
        for secret in [&b"SECRET=1"[..], b"not for anyone", b"[core]"] {
            assert!(
                !bytes.windows(secret.len()).any(|w| w == secret),
                "left out means not in the bytes either"
            );
        }
        // The times: the exact one in UTC, and ZIP's own field in local time.
        let entry = archive.by_name("Survey/notes.txt").unwrap();
        let utc = entry
            .extra_data_fields()
            .find_map(|f| match f {
                zip::ExtraField::ExtendedTimestamp(t) => t.mod_time(),
                _ => None,
            })
            .expect("the extended timestamp");
        assert_eq!(utc, 1_700_000_001);
        let dos = entry.last_modified().expect("a time");
        assert_eq!(Some(dos), local_time(when));
        assert_eq!(dos.second() % 2, 0);
        assert!(dos.year() == 2023 && dos.month() == 11);
        drop(entry);
        // The compressed data is deflate, and the permission bits are kept.
        let entry = archive.by_name("Survey/raw/day 1/a.bin").unwrap();
        assert_eq!(entry.compression(), CompressionMethod::Deflated);
        assert!(entry.compressed_size() < 200, "5000 equal bytes shrink");
        drop(entry);
        assert_eq!(
            archive
                .by_name("Survey/B.txt")
                .unwrap()
                .unix_mode()
                .map(|m| m & 0o777),
            Some(0o640)
        );

        // (To look at the archive with other tools: `unzip -t`, `python3 -m zipfile -l`.)
        if let Some(keep) = std::env::var_os("KOURIER_TEST_KEEP_ARCHIVE") {
            fs::copy(&dest, keep).unwrap();
        }

        // A second archive of the same folder gets its own place; a release removes one, a
        // clear all, and what is not ours is never touched.
        let again = store.reserve("Survey").unwrap();
        assert_ne!(again, dest);
        assert!(store.owns(&dest) && store.owns(&again));
        assert!(!store.owns(&root.join("notes.txt")));
        assert!(!store.release(&root.join("notes.txt")));
        assert!(root.join("notes.txt").exists());
        assert!(store.release(&dest));
        assert!(!dest.exists() && !dest.parent().unwrap().exists());
        assert!(!store.owns(&dest));
        // Forgetting and deleting can be two steps (the UI thread forgets, a worker
        // deletes): what was taken is no longer ours, and is gone once it was removed.
        let third = store.reserve("Survey").unwrap();
        fs::write(&third, b"PK").unwrap();
        assert_eq!(store.take(&root.join("notes.txt")), None);
        let place = store.take(&third).expect("ours");
        assert!(!store.owns(&third) && third.exists());
        assert_eq!(store.take(&third), None, "taken once");
        remove_place(&place);
        assert!(!third.exists() && !place.exists());
        remove_place(&place);
        fs::write(&again, b"PK").unwrap();
        store.clear();
        assert!(!again.exists());
        // The app is closing: a packing that is still running gets no new place.
        assert!(matches!(
            store.reserve("Too late"),
            Err(FolderError::NoStore)
        ));
        assert_eq!(
            fs::read_dir(dir.path().join("cache/outgoing"))
                .unwrap()
                .count(),
            0
        );
    }

    #[test]
    fn asking_about_an_archive_never_waits_for_a_place_being_made() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(ArchiveFiles::new(&dir.path().join("cache/outgoing")));
        let ours = store.reserve("Survey").unwrap();
        // A worker is making a place on a disk that does not answer (it holds the gate).
        let busy = store.gate.lock().unwrap();
        // The UI thread asks about a path and lets one go meanwhile: neither waits.
        let (tx, rx) = std::sync::mpsc::channel();
        let asked = {
            let (store, ours) = (store.clone(), ours.clone());
            std::thread::spawn(move || {
                let owned = store.owns(&ours);
                let place = store.take(&ours);
                let _ = tx.send((owned, place, store.owns(&ours)));
            })
        };
        let answer = rx.recv_timeout(Duration::from_secs(10));
        // Another worker that wants a place does wait for the first.
        let (tx2, rx2) = std::sync::mpsc::channel();
        let waiting = {
            let store = store.clone();
            std::thread::spawn(move || {
                let _ = tx2.send(store.reserve("Second").is_ok());
            })
        };
        let early = rx2.recv_timeout(Duration::from_millis(300));
        drop(busy);
        asked.join().unwrap();
        let (owned, place, still) = answer.expect("the list was asked while a place was made");
        assert!(owned && !still);
        assert_eq!(place.as_deref(), ours.parent());
        assert!(early.is_err(), "two places are not made at once");
        assert_eq!(rx2.recv_timeout(Duration::from_secs(10)), Ok(true));
        waiting.join().unwrap();
    }

    #[test]
    fn what_a_crashed_run_left_goes_at_the_next_start() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("cache/outgoing");
        let first = ArchiveFiles::new(&base);
        let left = first.reserve("Left behind").unwrap();
        fs::write(&left, b"PK\x03\x04").unwrap();
        // (A crash: nothing is cleared.)
        std::mem::forget(first);
        assert!(left.exists());
        let keep = dir.path().join("cache/other.txt");
        fs::write(&keep, b"not ours").unwrap();
        let second = ArchiveFiles::new(&base);
        assert!(!left.exists(), "the next start wipes the directory");
        assert!(keep.exists(), "only that directory");
        assert_eq!(
            fs::metadata(&base).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert!(!second.owns(&left));
        // Without a directory nothing is packed.
        let file = dir.path().join("a file");
        fs::write(&file, b"x").unwrap();
        let none = ArchiveFiles::new(&file.join("below a file"));
        assert!(matches!(none.reserve("x"), Err(FolderError::NoStore)));
    }

    #[test]
    fn empty_folders_and_odd_names() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("Nothing");
        fs::create_dir_all(root.join("sub")).unwrap();
        write(&root.join(".hidden"), b"x");
        let summary = summarize(&root, &Cancel::new(), &mut |_| {}).unwrap();
        assert_eq!(
            (summary.files, summary.bytes, summary.skipped.hidden),
            (0, 0, 1)
        );
        let dest = dir.path().join("nothing.zip");
        assert!(matches!(
            pack(&root, "Nothing", &dest, &Cancel::new(), &mut |_, _| {}),
            Err(FolderError::Empty)
        ));
        assert!(!dest.exists(), "no archive of nothing");
        // Archive names are plain file names, whatever the folder is called.
        assert_eq!(archive_name("Survey"), "Survey.zip");
        assert_eq!(archive_name("a/b\\c"), "a_b_c.zip");
        assert_eq!(archive_name(".."), "_.zip");
        assert_eq!(archive_name(""), "_.zip");
        let long = archive_name(&"ж".repeat(300));
        assert!(long.len() <= NAME_MAX && long.ends_with(".zip"));
        assert!(long.is_char_boundary(long.len() - 4));
        // Entry names never leave the archive's own folder.
        let mut used = HashSet::new();
        let root = Path::new("/home/u/Folder");
        assert_eq!(
            entry_name(
                "Folder",
                root,
                Path::new("/home/u/Folder/a/b.txt"),
                &mut used
            )
            .as_deref(),
            Some("Folder/a/b.txt")
        );
        assert_eq!(
            entry_name(
                "Folder",
                root,
                Path::new("/home/u/Folder/a/b.txt"),
                &mut used
            )
            .as_deref(),
            Some("Folder/a/b (2).txt")
        );
        assert_eq!(
            entry_name("Folder", root, Path::new("/home/u/Folder/../x"), &mut used),
            None
        );
        assert_eq!(
            entry_name("Folder", root, Path::new("/etc/passwd"), &mut used),
            None
        );
        assert_eq!(entry_name("Folder", root, root, &mut used), None);
        assert_eq!(numbered("a/b", 3), "a/b (3)");
        assert_eq!(numbered("a/.profile", 2), "a/.profile (2)");
    }

    #[test]
    fn sockets_and_pipes_are_left_out() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("Run");
        write(&root.join("pid"), b"42");
        let _socket = std::os::unix::net::UnixListener::bind(root.join("control.sock")).unwrap();
        let fifo = std::ffi::CString::new(root.join("pipe").as_os_str().as_bytes()).unwrap();
        // SAFETY: `fifo` is a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        let listing = list(&root, &Cancel::new()).unwrap();
        assert_eq!(below(&root, &listing), ["pid"]);
        assert_eq!(listing.skipped.special, 2);
    }
}
