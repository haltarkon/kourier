//! A plaintext copy written into a folder the user chose (architecture §5.8.2): the one place
//! where plaintext leaves the encrypted store for a path the user owns.
//!
//! One entry point, [`save_to`], over any [`Read`]; [`save_copy`] is a thin wrapper that opens a
//! blob. The rules, each with a test:
//!
//! - The name is ONE path component ([`check_name`]): no `/`, no NUL, not `.` or `..`, at most
//!   255 bytes. Nothing is joined to the folder's path: the folder is opened once and every later
//!   step names the file relative to that open directory (`openat`, `linkat`, `renameat`), so a
//!   folder renamed or swapped for a link after it was opened changes nothing. The folder's own
//!   path may be a symbolic link (`~/Downloads` often is): the user chose it. The file's NAME is
//!   never followed: a link of that name is replaced (or, in the no-replace mode, counts as a file
//!   that exists), whatever it points to.
//! - The data goes to an anonymous file first (`O_TMPFILE`: it has no name, so nothing half
//!   written can ever show under any name). Where the file system refuses that (FAT and exFAT
//!   sticks, some FUSE and network file systems) it goes to a hidden file `.kourier-<random>.part`
//!   created `O_EXCL | O_NOFOLLOW`, removed again on every error ([`Staging`]).
//! - The copy is published only after the reader returned `Ok(0)`. A [`BlobReader`] returns it
//!   only after every chunk authenticated, so a tampered or truncated blob leaves nothing.
//! - Never an executable bit: the file is created with the mode `0o666`, which the umask and the
//!   folder's default ACL can only narrow.
//! - The file is flushed (`fsync`) before it gets its name and the folder after.
//! - [`SaveMode::Replace`] puts the copy over what has the name, atomically (`rename`);
//!   [`SaveMode::NoReplace`] never does: a name that is taken is the typed answer
//!   [`SaveError::Exists`], decided by the file system in the same call that creates the name
//!   (`linkat`, or `renameat2` with `RENAME_NOREPLACE`). Where the file system has neither (the
//!   [`Staging::Named`] route on FAT), the existence is looked at first (the one race this
//!   crate leaves, on a file system that cannot do better).
//! - The log says that a copy was saved and how big it was, never a path or a name (§9.1).

use crate::{BlobError, BlobReader, FileKey};
use rand::RngCore;
use std::ffi::{CStr, CString};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use zeroize::Zeroizing;

/// The longest file name the writer takes, in bytes (the limit of most Linux file systems).
pub const MAX_NAME_BYTES: usize = 255;

/// What happens when a file of that name is in the folder already.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SaveMode {
    /// The copy takes its place (the user confirmed that in the file dialog).
    Replace,
    /// Nothing is replaced: [`SaveError::Exists`] (a folder written to without a question per
    /// file, "Save All").
    NoReplace,
}

/// How the data waits for its name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Staging {
    /// An anonymous `O_TMPFILE`; the named route below where the file system refuses it.
    Auto,
    /// A hidden named file and `rename`, the route of a file system without `O_TMPFILE`,
    /// `linkat` and `RENAME_NOREPLACE`. Forced by the tests of that route.
    Named,
}

/// Why a copy was not saved. Every message is log-safe (no path, no name).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SaveError {
    /// The name is not one path component (empty, `.`, `..`, a `/` or a NUL in it, over 255
    /// bytes).
    #[error("the file name is not a single path component")]
    BadName,
    /// [`SaveMode::NoReplace`] and a file of that name exists.
    #[error("a file of that name exists")]
    Exists,
    /// The folder does not exist or is not a folder.
    #[error("the folder does not exist")]
    NoFolder,
    /// The folder, or the file system, does not let this user write.
    #[error("the folder is not writable")]
    NotWritable,
    /// The file system is full (or the user's quota is used up).
    #[error("no space left on the device")]
    NoSpace,
    /// The source could not be read to its end: a blob that does not open, a chunk that fails
    /// to authenticate, a truncated file, or the reader's own error.
    #[error("the source could not be read: {0}")]
    Source(BlobError),
    /// Any other error of the file system.
    #[error("writing the copy failed: {0}")]
    Io(io::Error),
}

/// Decrypts the blob at `blob` with `key` into `dir/name` ([`save_to`] over a [`BlobReader`]).
/// The name is checked before the blob is opened. Returns the plaintext length.
pub fn save_copy(
    blob: &Path,
    key: &FileKey,
    dir: &Path,
    name: &str,
    mode: SaveMode,
) -> Result<u64, SaveError> {
    check_name(name)?;
    let mut reader = BlobReader::open(blob, key).map_err(SaveError::Source)?;
    save_to(dir, name, mode, &mut reader)
}

/// Copies everything `src` yields into the new file `dir/name`; returns its length. The file
/// appears under its name only after `src` returned `Ok(0)`. See the module's rules.
pub fn save_to(
    dir: &Path,
    name: &str,
    mode: SaveMode,
    src: &mut dyn Read,
) -> Result<u64, SaveError> {
    save_staged(dir, name, mode, Staging::Auto, src)
}

/// [`save_to`] with the staging chosen (the tests force [`Staging::Named`]).
pub fn save_staged(
    dir: &Path,
    name: &str,
    mode: SaveMode,
    staging: Staging,
    src: &mut dyn Read,
) -> Result<u64, SaveError> {
    check_name(name)?;
    let target = CString::new(name).map_err(|_| SaveError::BadName)?;
    let dirfd = open_dir(dir)?;
    // An early answer, no more: a taken name costs no decrypt, write and fsync of the whole
    // file (a batch asks up to 99 names). `linkat` / `RENAME_NOREPLACE` below still decide, so
    // the mode stays race-free.
    if mode == SaveMode::NoReplace && exists(&dirfd, &target)? {
        return Err(SaveError::Exists);
    }
    let (mut file, part) = stage(&dirfd, staging)?;
    let bytes = copy(src, &mut file)?;
    // The data is on the disk before the file has its name.
    sync(file.as_raw_fd())?;
    match part {
        Part::Anonymous => publish_anonymous(&dirfd, &file, &target, mode)?,
        Part::Named(hidden) => publish_named(&dirfd, hidden, &target, mode, staging)?,
    }
    // The name is on the disk too. A file system without `fsync` on folders is fine.
    sync_dir(&dirfd)?;
    tracing::info!(bytes, "a plaintext copy was saved outside the store");
    Ok(bytes)
}

/// Whether `name` can be a file name of the writer: ONE path component.
pub fn check_name(name: &str) -> Result<(), SaveError> {
    let ok = !name.is_empty()
        && name.len() <= MAX_NAME_BYTES
        && !name.contains(['/', '\0'])
        && name != "."
        && name != "..";
    ok.then_some(()).ok_or(SaveError::BadName)
}

/// What the data was written to.
enum Part {
    /// An `O_TMPFILE`: no name yet.
    Anonymous,
    /// A hidden file in the folder, removed on drop unless taken.
    Named(Hidden),
}

/// The hidden name of a staged file; unlinks it when dropped before it was published.
struct Hidden {
    dirfd: RawFd,
    name: Option<CString>,
}

impl Hidden {
    fn name(&self) -> &CStr {
        self.name.as_deref().unwrap_or_default()
    }

    /// The file got its final name: nothing to remove.
    fn published(mut self) {
        self.name = None;
    }
}

impl Drop for Hidden {
    fn drop(&mut self) {
        if let Some(name) = &self.name {
            // SAFETY: `dirfd` is the open folder, which outlives this value (it is dropped
            // after it in `save_staged`), and `name` is a valid NUL-terminated string.
            unsafe { libc::unlinkat(self.dirfd, name.as_ptr(), 0) };
        }
    }
}

fn open_dir(dir: &Path) -> Result<OwnedFd, SaveError> {
    let path = CString::new(dir.as_os_str().as_bytes()).map_err(|_| SaveError::NoFolder)?;
    // SAFETY: `path` is a valid NUL-terminated string.
    let fd = unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(match errno() {
            libc::ENOENT | libc::ENOTDIR | libc::ENAMETOOLONG | libc::ELOOP => SaveError::NoFolder,
            libc::EACCES | libc::EPERM => SaveError::NotWritable,
            other => SaveError::Io(io::Error::from_raw_os_error(other)),
        });
    }
    // SAFETY: `open` returned a new descriptor that nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Creates the file the data is written to.
fn stage(dirfd: &OwnedFd, staging: Staging) -> Result<(File, Part), SaveError> {
    if staging == Staging::Auto && Path::new("/proc/self/fd").is_dir() {
        // SAFETY: "." is a valid NUL-terminated string and `dirfd` an open folder.
        let fd = unsafe {
            libc::openat(
                dirfd.as_raw_fd(),
                c".".as_ptr(),
                libc::O_TMPFILE | libc::O_WRONLY | libc::O_CLOEXEC,
                0o666_u32,
            )
        };
        if fd >= 0 {
            // SAFETY: `openat` returned a new descriptor that nothing else owns.
            return Ok((unsafe { File::from_raw_fd(fd) }, Part::Anonymous));
        }
        match errno() {
            // The file system (or an old kernel) does not know `O_TMPFILE`: the named route.
            libc::EOPNOTSUPP | libc::EISDIR | libc::EINVAL | libc::ENOSYS => {}
            other => return Err(classify(other)),
        }
    }
    let mut random = [0u8; 16];
    rand::rng().fill_bytes(&mut random);
    let hex: String = random.iter().map(|b| format!("{b:02x}")).collect();
    let hidden = CString::new(format!(".kourier-{hex}.part")).map_err(|_| SaveError::BadName)?;
    // SAFETY: `hidden` is a valid NUL-terminated string and `dirfd` an open folder.
    let fd = unsafe {
        libc::openat(
            dirfd.as_raw_fd(),
            hidden.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o666_u32,
        )
    };
    if fd < 0 {
        return Err(classify(errno()));
    }
    // SAFETY: `openat` returned a new descriptor that nothing else owns.
    let file = unsafe { File::from_raw_fd(fd) };
    let part = Part::Named(Hidden {
        dirfd: dirfd.as_raw_fd(),
        name: Some(hidden),
    });
    Ok((file, part))
}

fn copy(src: &mut dyn Read, out: &mut File) -> Result<u64, SaveError> {
    let mut buf = Zeroizing::new(vec![0u8; 64 * 1024]);
    let mut total = 0u64;
    loop {
        let n = match src.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(SaveError::Source(BlobError::from_io(e))),
        };
        out.write_all(&buf[..n])
            .map_err(|e| match e.raw_os_error() {
                Some(code) => classify(code),
                None => SaveError::Io(e),
            })?;
        total += n as u64;
    }
    Ok(total)
}

fn sync(fd: RawFd) -> Result<(), SaveError> {
    // SAFETY: `fd` is an open descriptor of the caller.
    if unsafe { libc::fsync(fd) } == 0 {
        Ok(())
    } else {
        Err(classify(errno()))
    }
}

fn sync_dir(dirfd: &OwnedFd) -> Result<(), SaveError> {
    // SAFETY: `dirfd` is an open descriptor.
    if unsafe { libc::fsync(dirfd.as_raw_fd()) } == 0 {
        return Ok(());
    }
    match errno() {
        libc::EINVAL | libc::EROFS | libc::EOPNOTSUPP => Ok(()),
        other => Err(classify(other)),
    }
}

/// The anonymous file gets its name: `linkat` through `/proc/self/fd` (it needs no privilege,
/// unlike `AT_EMPTY_PATH`). In the replace mode the copy first gets a hidden name and then
/// `rename` puts it over the target in one step.
fn publish_anonymous(
    dirfd: &OwnedFd,
    file: &File,
    target: &CStr,
    mode: SaveMode,
) -> Result<(), SaveError> {
    let source = CString::new(format!("/proc/self/fd/{}", file.as_raw_fd()))
        .map_err(|_| SaveError::BadName)?;
    let link = |name: &CStr| -> Result<(), SaveError> {
        // SAFETY: both paths are valid NUL-terminated strings, `dirfd` is an open folder;
        // `AT_SYMLINK_FOLLOW` follows the `/proc` link of the descriptor (the old path), never
        // the new name.
        let rc = unsafe {
            libc::linkat(
                libc::AT_FDCWD,
                source.as_ptr(),
                dirfd.as_raw_fd(),
                name.as_ptr(),
                libc::AT_SYMLINK_FOLLOW,
            )
        };
        if rc == 0 {
            Ok(())
        } else {
            Err(classify(errno()))
        }
    };
    match mode {
        // `linkat` refuses a taken name (a link of that name too, dangling or not) with EEXIST.
        SaveMode::NoReplace => link(target),
        SaveMode::Replace => {
            let mut random = [0u8; 16];
            rand::rng().fill_bytes(&mut random);
            let hex: String = random.iter().map(|b| format!("{b:02x}")).collect();
            let hidden =
                CString::new(format!(".kourier-{hex}.part")).map_err(|_| SaveError::BadName)?;
            link(&hidden)?;
            let named = Hidden {
                dirfd: dirfd.as_raw_fd(),
                name: Some(hidden),
            };
            rename_over(dirfd, named.name(), target)?;
            named.published();
            Ok(())
        }
    }
}

/// The hidden file gets its name.
fn publish_named(
    dirfd: &OwnedFd,
    hidden: Hidden,
    target: &CStr,
    mode: SaveMode,
    staging: Staging,
) -> Result<(), SaveError> {
    match mode {
        SaveMode::Replace => rename_over(dirfd, hidden.name(), target)?,
        SaveMode::NoReplace => {
            if staging == Staging::Auto {
                // `RENAME_NOREPLACE` decides in the same call that gives the name.
                match rename_no_replace(dirfd, hidden.name(), target) {
                    Ok(()) => {
                        hidden.published();
                        return Ok(());
                    }
                    // This file system does not know the flag: look first, below.
                    Err(libc::EINVAL | libc::ENOSYS | libc::EOPNOTSUPP) => {}
                    Err(code) => return Err(classify(code)),
                }
            }
            if exists(dirfd, target)? {
                return Err(SaveError::Exists);
            }
            rename_over(dirfd, hidden.name(), target)?;
        }
    }
    hidden.published();
    Ok(())
}

/// `renameat2(RENAME_NOREPLACE)`; the `errno` on failure.
fn rename_no_replace(dirfd: &OwnedFd, from: &CStr, to: &CStr) -> Result<(), i32> {
    // SAFETY: both names are valid NUL-terminated strings and `dirfd` is an open folder; the
    // call takes no other pointer.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            dirfd.as_raw_fd(),
            from.as_ptr(),
            dirfd.as_raw_fd(),
            to.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if rc == 0 { Ok(()) } else { Err(errno()) }
}

/// Whether a file system entry of that name is there. A link counts, dangling or not: it is
/// looked at, not followed.
fn exists(dirfd: &OwnedFd, name: &CStr) -> Result<bool, SaveError> {
    // SAFETY: `stat` is plain old data, so all-zero is a valid value.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: `name` is a valid NUL-terminated string, `st` a valid out pointer.
    let rc = unsafe {
        libc::fstatat(
            dirfd.as_raw_fd(),
            name.as_ptr(),
            &mut st,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if rc == 0 {
        return Ok(true);
    }
    match errno() {
        libc::ENOENT => Ok(false),
        other => Err(classify(other)),
    }
}

fn rename_over(dirfd: &OwnedFd, from: &CStr, to: &CStr) -> Result<(), SaveError> {
    // SAFETY: both names are valid NUL-terminated strings and `dirfd` is an open folder.
    let rc = unsafe {
        libc::renameat(
            dirfd.as_raw_fd(),
            from.as_ptr(),
            dirfd.as_raw_fd(),
            to.as_ptr(),
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(classify(errno()))
    }
}

fn errno() -> i32 {
    io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// The class of an `errno` the writer can meet.
fn classify(code: i32) -> SaveError {
    match code {
        libc::EEXIST => SaveError::Exists,
        libc::ENOSPC | libc::EDQUOT => SaveError::NoSpace,
        libc::EACCES | libc::EPERM | libc::EROFS => SaveError::NotWritable,
        other => SaveError::Io(io::Error::from_raw_os_error(other)),
    }
}
