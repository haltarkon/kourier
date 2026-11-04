//! What the application decides about Telegram before its backend is made (TG-2, shared commit
//! S-7; the critic's H3 and M13, his Q2 answer "Run-tool profile only"):
//!
//! - [`parts_state`]: whether this build's Telegram parts can log in at all (an API pair AND the
//!   helper files), which `app::telegram_registration` turns into Registered / Listed /
//!   Unavailable / Absent.
//! - [`run_profile`]: the debug build's KEPT RUN PROFILE: `KOURIER_HOME` set, canonical, and a
//!   direct child of `<XDG_DATA_HOME or HOME/.local/share>/kourier-tg-run/` (the keeper root of
//!   the Telegram run tool, mirroring xtask's `keeper_root` of `smoke` and `wa-run`).
//! - [`dev_api`]: the developer's API pair, read ONLY in a debug build inside such a profile from
//!   the FIXED file `<keeper root>/telegram-api` (0600, owned by this user, its directory 0700
//!   and owned by this user; two lines `id=<digits>` and `hash=<32 lower-case hex digits>`). The
//!   file is not the safety boundary: the dialer refuses every dial unless the run tool's
//!   confirmation ([`dial_confirmation`]) chose the TCP dialer.
//! - [`servers`]: `KOURIER_DEV_TELEGRAM_ENV` (`production`, the default, or `test` for Telegram's
//!   test servers): `test` only in a debug build inside the run profile; anything else fails the
//!   start typed.
//! - [`dial_confirmation`] (TR R0a, D-10): whether this start was confirmed by the run tool to dial
//!   Telegram's TEST servers: the only place of the application that names the dialing mode.
//!
//! Nothing here opens a connection, starts a process or loads a library; nothing it reads reaches
//! a log line (the problems are fixed words).

use std::ffi::OsString;
use std::io::Read as _;
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};

use kourier_telegram::real::DialMode;
use kourier_telegram::{ApiCredentials, HelperFiles, TgEnv};
use zeroize::Zeroizing;

use crate::config::ConfigError;
use crate::paths::HOME_ENV;

/// The directory of the Telegram run tool's kept profiles below the data directory.
pub(crate) const RUN_DIR: &str = "kourier-tg-run";
/// The developer's API file in the keeper root.
pub(crate) const API_FILE: &str = "telegram-api";
/// The debug switch of Telegram's servers.
pub const TELEGRAM_ENV: &str = "KOURIER_DEV_TELEGRAM_ENV";
/// The API file is two short lines: anything longer is not one.
const API_FILE_MAX: u64 = 256;
/// The variable that carries the run tool's nonce to the app (TR R0a, D-10).
#[cfg_attr(not(debug_assertions), allow(dead_code))]
pub(crate) const DIAL_ENV: &str = "KOURIER_DEV_TELEGRAM_DIAL";
/// The directory of the run profile that holds the confirmation file.
#[cfg_attr(not(debug_assertions), allow(dead_code))]
pub(crate) const DIAL_DIR: &str = "tg-run";
/// The confirmation file the run tool writes right before it starts the app.
#[cfg_attr(not(debug_assertions), allow(dead_code))]
pub(crate) const DIAL_FILE: &str = "dial-go.json";
/// The confirmation file is one short line of JSON: anything longer is not one.
#[cfg(debug_assertions)]
const DIAL_FILE_MAX: u64 = 1024;
/// How old a confirmation may be, in milliseconds.
#[cfg(debug_assertions)]
const DIAL_FRESH_MS: u64 = 120_000;
/// How far ahead of this computer's clock a confirmation may be dated, in milliseconds.
#[cfg(debug_assertions)]
const DIAL_AHEAD_MS: u64 = 5_000;

/// What a Telegram account of this build cannot run without and does not have: the type lives
/// in [`crate::app`] (always compiled, KT-1b C5a: the front-end's code names it without a cfg).
pub use crate::app::TelegramMissing;

/// Whether this build's Telegram parts can log in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PartsState {
    /// An API pair and both helper files exist.
    Ready,
    /// Something a login needs is missing.
    Missing(TelegramMissing),
}

/// The state of parts with (`has_api`) or without an API pair over `files`. Only looks at which
/// files exist (the pair first: without one nothing else matters).
pub(crate) fn parts_state(has_api: bool, files: &HelperFiles) -> PartsState {
    if !has_api {
        PartsState::Missing(TelegramMissing::ApiKey)
    } else if !(files.helper.is_file() && files.tdjson.is_file()) {
        PartsState::Missing(TelegramMissing::Library)
    } else {
        PartsState::Ready
    }
}

/// `${XDG_DATA_HOME:-$HOME/.local/share}/kourier-tg-run` (relative values ignored, as xtask's
/// `keeper_root` does); `None` without either variable.
pub(crate) fn keeper_root(env: &dyn Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    let abs = |k: &str| {
        env(k)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
    };
    let data = abs("XDG_DATA_HOME").or_else(|| abs("HOME").map(|h| h.join(".local/share")))?;
    Some(data.join(RUN_DIR))
}

/// The keeper root (canonical) when this process runs in a kept Telegram run profile:
/// `KOURIER_HOME` is set, absolute, already canonical (no `..`, no symbolic link on the way) and
/// a DIRECT child of the canonical keeper root. `None` in every other case, also when either
/// directory does not exist.
pub(crate) fn run_profile(env: &dyn Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    let home = PathBuf::from(env(HOME_ENV).filter(|v| !v.is_empty())?);
    if !home.is_absolute() {
        return None;
    }
    let canonical = std::fs::canonicalize(&home).ok()?;
    if canonical != home {
        return None;
    }
    let root = std::fs::canonicalize(keeper_root(env)?).ok()?;
    (home.parent() == Some(root.as_path())).then_some(root)
}

/// Why the developer's API file is not used (fixed words: never the file's content or path).
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum ApiFileError {
    /// It is a symbolic link, a directory or another kind of file.
    #[error("the Telegram API file is not a regular file")]
    NotAFile,
    /// Not owned by this user.
    #[error("the Telegram API file is not owned by this user")]
    Owner,
    /// Readable or writable by others (not 0600).
    #[error("the Telegram API file is not private (mode 0600)")]
    Mode,
    /// Its directory is not this user's private directory (0700).
    #[error("the directory of the Telegram API file is not private (mode 0700, this user's)")]
    Parent,
    /// It could not be read.
    #[error("the Telegram API file could not be read")]
    Read,
    /// Not the two lines `id=<digits>` and `hash=<32 lower-case hex digits>`.
    #[error("the Telegram API file is not two lines id=... and hash=...")]
    Form,
}

/// The pair in `<root>/telegram-api`: `Ok(None)` when there is no such file. The file is opened
/// without following a link (`O_NOFOLLOW`) and checked on its open descriptor.
pub(crate) fn read_api_file(root: &Path) -> Result<Option<ApiCredentials>, ApiFileError> {
    // SAFETY: getuid has no preconditions and cannot fail.
    let uid = unsafe { libc::getuid() };
    read_api_file_as(root, uid, uid)
}

/// [`read_api_file`] with the uids the directory and the file must belong to. Both are this
/// user's in the application; a test passes another one, because the owner checks cannot be
/// reached by a plain user otherwise (the privacy review's R3: a mutant of one survived).
fn read_api_file_as(
    root: &Path,
    dir_uid: u32,
    file_uid: u32,
) -> Result<Option<ApiCredentials>, ApiFileError> {
    let dir = std::fs::symlink_metadata(root).map_err(|_| ApiFileError::Parent)?;
    if !dir.is_dir() || dir.uid() != dir_uid || dir.mode() & 0o777 != 0o700 {
        return Err(ApiFileError::Parent);
    }
    let path = root.join(API_FILE);
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&path);
    let mut file = match file {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        // O_NOFOLLOW on a symbolic link: ELOOP.
        Err(e) if e.raw_os_error() == Some(libc::ELOOP) => return Err(ApiFileError::NotAFile),
        Err(_) => return Err(ApiFileError::Read),
    };
    let meta = file.metadata().map_err(|_| ApiFileError::Read)?;
    if !meta.file_type().is_file() {
        return Err(ApiFileError::NotAFile);
    }
    if meta.uid() != file_uid {
        return Err(ApiFileError::Owner);
    }
    if meta.mode() & 0o777 != 0o600 {
        return Err(ApiFileError::Mode);
    }
    if meta.len() > API_FILE_MAX {
        return Err(ApiFileError::Form);
    }
    let mut text = Zeroizing::new(String::with_capacity(API_FILE_MAX as usize + 1));
    (&mut file)
        .take(API_FILE_MAX + 1)
        .read_to_string(&mut text)
        .map_err(|_| ApiFileError::Read)?;
    parse_api_file(&text).map(Some)
}

/// The two lines `id=<digits>` and `hash=<hex>` (in this order; a trailing newline and blank
/// lines are fine, nothing else is).
fn parse_api_file(text: &str) -> Result<ApiCredentials, ApiFileError> {
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
    let id = lines
        .next()
        .and_then(|l| l.strip_prefix("id="))
        .ok_or(ApiFileError::Form)?;
    let hash = lines
        .next()
        .and_then(|l| l.strip_prefix("hash="))
        .ok_or(ApiFileError::Form)?;
    if lines.next().is_some() {
        return Err(ApiFileError::Form);
    }
    ApiCredentials::from_text(id, hash).ok_or(ApiFileError::Form)
}

/// The developer's API pair: only when `debug` and only inside a run profile ([`run_profile`]),
/// from its keeper root's fixed file. A file that is there but unusable is told once as a warning
/// (its fixed word) and not used.
pub(crate) fn dev_api(
    debug: bool,
    env: &dyn Fn(&str) -> Option<OsString>,
) -> Option<ApiCredentials> {
    if !debug {
        return None;
    }
    let root = run_profile(env)?;
    match read_api_file(&root) {
        Ok(pair) => pair,
        Err(problem) => {
            tracing::warn!(%problem, "the developer's Telegram API file is not used");
            None
        }
    }
}

/// Telegram's servers for a NEW account from [`TELEGRAM_ENV`]: unset, empty or `production` ->
/// production in every build; `test` -> the test servers, only when `debug` AND inside a run
/// profile (his Q2 answer); any other value, and `test` elsewhere, fails the start typed.
pub(crate) fn servers(
    debug: bool,
    env: &dyn Fn(&str) -> Option<OsString>,
) -> Result<TgEnv, ConfigError> {
    let value = env(TELEGRAM_ENV).unwrap_or_default();
    match value.to_str().map(str::trim) {
        Some("" | "production") => Ok(TgEnv::Production),
        Some("test") if !debug => Err(ConfigError::Telegram(
            "Telegram's test servers are not available in a release build",
        )),
        Some("test") if run_profile(env).is_none() => Err(ConfigError::Telegram(
            "Telegram's test servers are used only in a kept run profile",
        )),
        Some("test") => Ok(TgEnv::Test),
        _ => Err(ConfigError::Telegram(
            "KOURIER_DEV_TELEGRAM_ENV is not valid (production, or test in a run profile)",
        )),
    }
}

/// Why a start does not dial Telegram: the fixed word of the INFO line `telegram dialer`.
#[cfg_attr(not(debug_assertions), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// A release build (also the answer of a debug build that was asked as one).
    Release,
    /// Not a kept run profile (`KOURIER_HOME` is not a direct child of the keeper root).
    NoProfile,
    /// The run tool's variable is not set: the plain start of a developer. The file is not touched.
    NoConfirmation,
    /// The confirmation file is missing or is not what the run tool writes.
    File,
    /// The file is older than 120 s or dated more than 5 s ahead.
    Stale,
    /// The variable's hash is not the file's.
    Mismatch,
    /// Not Telegram's test servers (the app's own switch AND the file's).
    Env,
    /// No expected number tail: a login could link any account.
    NoTail,
}

impl Refusal {
    /// The word the INFO line carries.
    pub(crate) const fn word(self) -> &'static str {
        match self {
            Self::Release => "release",
            Self::NoProfile => "no-profile",
            Self::NoConfirmation => "no-confirmation",
            Self::File => "file",
            Self::Stale => "stale",
            Self::Mismatch => "mismatch",
            Self::Env => "env",
            Self::NoTail => "no-tail",
        }
    }
}

/// The reason word of a confirmed start.
pub(crate) const CONFIRMED: &str = "confirmed";

/// What a start does about dialing Telegram, and why (both are fixed words in the log).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DialDecision {
    /// The dialer the start's parts carry.
    pub(crate) mode: DialMode,
    /// A [`Refusal::word`], or [`CONFIRMED`].
    pub(crate) reason: &'static str,
}

impl DialDecision {
    /// Tells the decision once, as an INFO line of fixed words (never a path, a value or a hash).
    pub(crate) fn announce(&self) {
        tracing::info!(
            mode = self.mode.word(),
            reason = self.reason,
            "telegram dialer"
        );
    }
}

/// Whether THIS start was confirmed by the run tool to dial Telegram (TR R0a, D-10). The only
/// function of the application that names the dialing mode (a source check pins that). The answer
/// is `Telegram` only when ALL of these hold, judged in this order:
///
/// - `debug`, and (only a debug build has the reader at all) the process runs in a kept run
///   profile ([`run_profile`]);
/// - the run tool's variable [`DIAL_ENV`] is set (a plain start leaves the file alone);
/// - the file `<profile>/tg-run/dial-go.json` is there, taken ONCE: opened without following a
///   link and unlinked at once, then judged on the descriptor (a regular file of this user at 0600
///   of at most 1 KiB in a 0700 directory of this user; exactly the run tool's fields);
/// - it is no older than 120 s and no more than 5 s ahead of `now_ms`;
/// - the SHA-256 of the variable is the file's hash (compared in constant time);
/// - Telegram's TEST servers are selected ([`servers`]) and the file says `test` too;
/// - the expected number tail is set.
///
/// Anything else refuses. A second call in the same process finds no file and refuses.
pub(crate) fn dial_confirmation(
    debug: bool,
    env: &dyn Fn(&str) -> Option<OsString>,
    now_ms: u64,
) -> DialDecision {
    // SAFETY: getuid has no preconditions and cannot fail.
    let uid = unsafe { libc::getuid() };
    match dial_confirmation_as(debug, env, now_ms, uid, uid) {
        Ok(()) => DialDecision {
            mode: DialMode::Telegram,
            reason: CONFIRMED,
        },
        Err(no) => DialDecision {
            mode: DialMode::Refuse,
            reason: no.word(),
        },
    }
}

/// [`dial_confirmation`]'s judgment with the uids the directory and the file must belong to. Both
/// are this user's in the application; a test passes another one, because the owner checks cannot
/// be reached by a plain user otherwise. It names no dialing mode: `Ok` is the confirmation.
pub(crate) fn dial_confirmation_as(
    debug: bool,
    env: &dyn Fn(&str) -> Option<OsString>,
    now_ms: u64,
    dir_uid: u32,
    file_uid: u32,
) -> Result<(), Refusal> {
    if !debug {
        return Err(Refusal::Release);
    }
    #[cfg(debug_assertions)]
    {
        judge(env, now_ms, dir_uid, file_uid)
    }
    #[cfg(not(debug_assertions))]
    {
        let _ = (env, now_ms, dir_uid, file_uid);
        Err(Refusal::Release)
    }
}

/// What the run tool wrote: exactly these fields.
#[cfg(debug_assertions)]
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct DialFile {
    version: u64,
    issued_ms: u64,
    env: String,
    nonce_sha256: String,
}

/// The judgment of a debug build (see [`dial_confirmation`] for the order and the reasons).
#[cfg(debug_assertions)]
fn judge(
    env: &dyn Fn(&str) -> Option<OsString>,
    now_ms: u64,
    dir_uid: u32,
    file_uid: u32,
) -> Result<(), Refusal> {
    run_profile(env).ok_or(Refusal::NoProfile)?;
    let home = env(HOME_ENV).map(PathBuf::from).ok_or(Refusal::NoProfile)?;
    // Set or not: a plain start never touches the file. Set: the file is consumed from here on,
    // whatever the verdict.
    let nonce = Zeroizing::new(
        env(DIAL_ENV)
            .ok_or(Refusal::NoConfirmation)?
            .into_encoded_bytes(),
    );
    let file = take_dial_file(&home.join(DIAL_DIR), dir_uid, file_uid)?;
    if now_ms > file.issued_ms.saturating_add(DIAL_FRESH_MS)
        || file.issued_ms > now_ms.saturating_add(DIAL_AHEAD_MS)
    {
        return Err(Refusal::Stale);
    }
    if !same_text(&sha256_hex(&nonce), &file.nonce_sha256) {
        return Err(Refusal::Mismatch);
    }
    if !matches!(servers(true, env), Ok(TgEnv::Test)) || file.env != "test" {
        return Err(Refusal::Env);
    }
    let as_text = |k: &str| env(k).and_then(|v| v.into_string().ok());
    if !matches!(crate::dev::expected_number_tail(as_text), Ok(Some(_))) {
        return Err(Refusal::NoTail);
    }
    Ok(())
}

/// Takes the confirmation file out of `dir` (the run profile's `tg-run`): the directory is opened
/// as a directory without following a link and checked on its descriptor, the file is opened
/// relative to it (no link, no blocking: a FIFO opens), UNLINKED at once, and only then judged on
/// its open descriptor and parsed. A file that cannot be unlinked is refused, and so is one that
/// had another name (its link count is not 0 after the unlink), so a confirmation is never usable
/// twice.
#[cfg(debug_assertions)]
fn take_dial_file(dir: &Path, dir_uid: u32, file_uid: u32) -> Result<DialFile, Refusal> {
    use std::os::fd::{AsRawFd as _, FromRawFd as _};

    let dir = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(dir)
        .map_err(|_| Refusal::File)?;
    let meta = dir.metadata().map_err(|_| Refusal::File)?;
    if !meta.is_dir() || meta.uid() != dir_uid || meta.mode() & 0o777 != 0o700 {
        return Err(Refusal::File);
    }
    let name = std::ffi::CString::new(DIAL_FILE).map_err(|_| Refusal::File)?;
    // SAFETY: `name` is a valid NUL-terminated string that outlives the call and the directory
    // descriptor is open for the whole function.
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(Refusal::File);
    }
    // SAFETY: `fd` is a fresh descriptor nobody else owns.
    let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
    // SAFETY: the same valid name and directory descriptor.
    if unsafe { libc::unlinkat(dir.as_raw_fd(), name.as_ptr(), 0) } != 0 {
        return Err(Refusal::File);
    }
    let meta = file.metadata().map_err(|_| Refusal::File)?;
    // `st_nlink == 0`: the unlink above removed the file's ONLY name. A file with another name (a
    // hard link in the profile) leaves that name behind, and the same bytes would stay usable as a
    // confirmation (TR R0a fix G-6, review F6).
    if !meta.file_type().is_file()
        || meta.uid() != file_uid
        || meta.mode() & 0o777 != 0o600
        || meta.len() > DIAL_FILE_MAX
        || meta.nlink() != 0
    {
        return Err(Refusal::File);
    }
    let mut bytes = Vec::with_capacity(DIAL_FILE_MAX as usize + 1);
    (&mut file)
        .take(DIAL_FILE_MAX + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| Refusal::File)?;
    let parsed: DialFile = serde_json::from_slice(&bytes).map_err(|_| Refusal::File)?;
    let hash = &parsed.nonce_sha256;
    let hex = hash.len() == 64 && hash.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    if parsed.version != 1 || !hex {
        return Err(Refusal::File);
    }
    Ok(parsed)
}

/// The lower-case hex SHA-256 of `bytes`.
#[cfg(debug_assertions)]
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest as _, Sha256};
    use std::fmt::Write as _;

    Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut out, b| {
            let _ = write!(out, "{b:02x}");
            out
        })
}

/// Whether two texts are equal, looking at every byte of equal-length texts (no early exit).
#[cfg(debug_assertions)]
fn same_text(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |diff, (x, y)| diff | (x ^ y))
            == 0
}

/// Test support shared with `app.rs`' tests of a start: the events a closure makes.
#[cfg(test)]
pub(crate) mod capture {
    /// Every event `f` makes on THIS thread, as `(level, fields)` (a subscriber of the calling
    /// thread only; the fields are the events' own words, no file content ever reaches one).
    pub(crate) fn events_of<T>(f: impl FnOnce() -> T) -> (T, Vec<(tracing::Level, String)>) {
        use std::sync::{Arc, Mutex};

        use tracing_subscriber::layer::SubscriberExt as _;

        type Seen = Arc<Mutex<Vec<(tracing::Level, String)>>>;

        struct Capture(Seen);

        impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Capture {
            fn on_event(
                &self,
                event: &tracing::Event<'_>,
                _: tracing_subscriber::layer::Context<'_, S>,
            ) {
                struct Fields(String);
                impl tracing::field::Visit for Fields {
                    fn record_debug(&mut self, f: &tracing::field::Field, v: &dyn std::fmt::Debug) {
                        use std::fmt::Write as _;
                        let _ = write!(self.0, "{}={:?} ", f.name(), v);
                    }
                }
                let mut fields = Fields(String::new());
                event.record(&mut fields);
                self.0
                    .lock()
                    .expect("lock")
                    .push((*event.metadata().level(), fields.0));
            }
        }

        let seen = Seen::default();
        let subscriber = tracing_subscriber::registry().with(Capture(seen.clone()));
        let out = tracing::subscriber::with_default(subscriber, f);
        let events = seen.lock().expect("lock").clone();
        (out, events)
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::capture::events_of;
    use super::*;

    /// A scratch data directory with the keeper root `data/kourier-tg-run` (0700) and the profile
    /// `p` below it; the environment names the data directory and `home` as `KOURIER_HOME`.
    struct Scratch {
        _tmp: tempfile::TempDir,
        data: PathBuf,
        root: PathBuf,
        profile: PathBuf,
    }

    fn scratch() -> Scratch {
        let tmp = tempfile::tempdir().expect("tmp");
        // The temp dir itself may sit below a symbolic link (/tmp): work on its canonical path.
        let base = std::fs::canonicalize(tmp.path()).expect("canonical");
        let data = base.join("data");
        let root = data.join(RUN_DIR);
        let profile = root.join("p");
        std::fs::create_dir_all(&profile).expect("dirs");
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).expect("chmod");
        Scratch {
            _tmp: tmp,
            data,
            root,
            profile,
        }
    }

    fn env_of(pairs: Vec<(&'static str, OsString)>) -> impl Fn(&str) -> Option<OsString> {
        move |k| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.clone())
    }

    fn in_profile(
        s: &Scratch,
        extra: Vec<(&'static str, OsString)>,
    ) -> impl Fn(&str) -> Option<OsString> {
        let mut pairs = vec![
            ("XDG_DATA_HOME", s.data.clone().into_os_string()),
            (HOME_ENV, s.profile.clone().into_os_string()),
        ];
        pairs.extend(extra);
        env_of(pairs)
    }

    /// The pair's text, built at run time (the source check refuses a 32-digit hex literal and a
    /// quoted number in a `parse` call): id 12345, hash `0123456789abcdef` twice.
    fn api_text() -> String {
        let id = ["123", "45"].concat();
        let hash = "0123456789abcdef".repeat(2);
        format!("id={id}\nhash={hash}\n")
    }

    fn write_api(s: &Scratch, text: &str, mode: u32) {
        let path = s.root.join(API_FILE);
        // A file left at 0400 by the loop before cannot be written again: write it anew.
        let _ = std::fs::remove_file(&path);
        std::fs::write(&path, text).expect("write");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).expect("chmod");
    }

    /// M13: the keeper root follows XDG_DATA_HOME, else HOME, and ignores relative values (as
    /// xtask's `keeper_root` of `smoke` and `wa-run`; the run tool `tg-run` does not exist yet).
    #[test]
    fn the_keeper_root_follows_xdg_data_home() {
        let xdg = env_of(vec![("XDG_DATA_HOME", "/d".into()), ("HOME", "/h".into())]);
        assert_eq!(keeper_root(&xdg), Some(PathBuf::from("/d/kourier-tg-run")));
        let home = env_of(vec![("XDG_DATA_HOME", "rel".into()), ("HOME", "/h".into())]);
        assert_eq!(
            keeper_root(&home),
            Some(PathBuf::from("/h/.local/share/kourier-tg-run"))
        );
        assert_eq!(keeper_root(&env_of(Vec::new())), None);
    }

    /// M13: only a canonical DIRECT child of the keeper root is a run profile.
    /// Mutant: `run_profile` returns the root whenever `KOURIER_HOME` is set -> fails.
    #[test]
    fn the_run_profile_is_a_canonical_direct_child_of_the_keeper_root() {
        let s = scratch();
        assert_eq!(
            run_profile(&in_profile(&s, Vec::new())),
            Some(s.root.clone())
        );
        let at = |home: PathBuf| {
            run_profile(&env_of(vec![
                ("XDG_DATA_HOME", s.data.clone().into_os_string()),
                (HOME_ENV, home.into_os_string()),
            ]))
        };
        assert_eq!(at(s.root.clone()), None, "the keeper root itself");
        let deeper = s.profile.join("q");
        std::fs::create_dir_all(&deeper).expect("dir");
        assert_eq!(at(deeper), None, "a grandchild");
        assert_eq!(at(s.profile.join("..").join("p")), None, "a path with ..");
        let elsewhere = s.data.join("other");
        std::fs::create_dir_all(&elsewhere).expect("dir");
        assert_eq!(at(elsewhere.clone()), None, "outside the keeper root");
        let link = s.root.join("link");
        std::os::unix::fs::symlink(&elsewhere, &link).expect("symlink");
        assert_eq!(at(link), None, "a symbolic link into the keeper root");
        assert_eq!(
            at(s.root.join("missing")),
            None,
            "a profile that does not exist"
        );
        assert_eq!(
            run_profile(&env_of(vec![(
                "XDG_DATA_HOME",
                s.data.clone().into_os_string()
            )])),
            None,
            "no KOURIER_HOME"
        );
    }

    /// His Q2 answer: Telegram's test servers only in a debug build inside the run profile.
    /// Mutant: the `run_profile` arm of `servers` removed -> the outside case gets `Test`.
    #[test]
    fn the_test_environment_is_refused_outside_the_run_profile() {
        let s = scratch();
        let test = || vec![(TELEGRAM_ENV, OsString::from("test"))];
        assert_eq!(
            servers(true, &in_profile(&s, test())).ok(),
            Some(TgEnv::Test)
        );
        assert!(servers(false, &in_profile(&s, test())).is_err(), "release");
        let outside = env_of(vec![
            ("XDG_DATA_HOME", s.data.clone().into_os_string()),
            (HOME_ENV, s.root.clone().into_os_string()),
            (TELEGRAM_ENV, "test".into()),
        ]);
        assert!(servers(true, &outside).is_err(), "outside the run profile");
        for debug in [true, false] {
            assert_eq!(
                servers(debug, &env_of(Vec::new())).ok(),
                Some(TgEnv::Production)
            );
            let prod = env_of(vec![(TELEGRAM_ENV, "production".into())]);
            assert_eq!(servers(debug, &prod).ok(), Some(TgEnv::Production));
            let bad = in_profile(&s, vec![(TELEGRAM_ENV, OsString::from("staging"))]);
            assert!(servers(debug, &bad).is_err(), "an unknown value");
        }
    }

    /// M13: the developer's pair is read only in a debug build inside the run profile, only from
    /// a private regular file of this user in a private directory, only in its two-line form.
    /// Mutants: the mode check removed -> the 0644 case is read; the `debug` check removed -> the
    /// release case gets the pair.
    #[test]
    fn the_dev_api_file_is_read_only_in_the_run_profile_and_only_0600() {
        let s = scratch();
        let env = in_profile(&s, Vec::new());
        assert!(dev_api(true, &env).is_none(), "no file yet");
        assert_eq!(read_api_file(&s.root).map(|p| p.is_some()), Ok(false));
        write_api(&s, &api_text(), 0o600);
        let pair = dev_api(true, &env).expect("the pair");
        assert_eq!(pair.id(), 12345);
        assert!(!pair.is_dummy());
        assert!(dev_api(false, &env).is_none(), "a release build read it");
        let outside = env_of(vec![
            ("XDG_DATA_HOME", s.data.clone().into_os_string()),
            (HOME_ENV, s.root.clone().into_os_string()),
        ]);
        assert!(
            dev_api(true, &outside).is_none(),
            "read outside the run profile"
        );
        for mode in [0o644, 0o640, 0o604, 0o700, 0o400] {
            write_api(&s, &api_text(), mode);
            assert_eq!(
                read_api_file(&s.root).map(|p| p.is_some()),
                Err(ApiFileError::Mode),
                "{mode:o}"
            );
        }
        write_api(&s, &api_text(), 0o600);
        std::fs::set_permissions(&s.root, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        assert_eq!(
            read_api_file(&s.root).map(|p| p.is_some()),
            Err(ApiFileError::Parent)
        );
        std::fs::set_permissions(&s.root, std::fs::Permissions::from_mode(0o700)).expect("chmod");
        let hash_only = format!("hash={}\n", "0123456789abcdef".repeat(2));
        let ids = ["123", "45"].concat();
        for bad in [
            String::new(),
            hash_only,
            format!("{}extra\n", api_text()),
            api_text().replace("id=", "key="),
            api_text().to_uppercase(),
            format!("id={ids}\n"),
        ] {
            write_api(&s, &bad, 0o600);
            assert_eq!(
                read_api_file(&s.root).map(|p| p.is_some()),
                Err(ApiFileError::Form)
            );
        }
        let real = s.data.join("elsewhere");
        std::fs::write(&real, api_text()).expect("write");
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o600)).expect("chmod");
        std::fs::remove_file(s.root.join(API_FILE)).expect("rm");
        std::os::unix::fs::symlink(&real, s.root.join(API_FILE)).expect("symlink");
        assert_eq!(
            read_api_file(&s.root).map(|p| p.is_some()),
            Err(ApiFileError::NotAFile)
        );
        // Nothing of the file reaches the problem's words.
        let words = format!("{}", ApiFileError::Form);
        assert!(!words.contains("12345") && !words.contains("0123456789abcdef"));
    }

    /// The privacy review's R3 (fix J15): the file's and its directory's OWNER are checked. A plain
    /// user has no second uid, so the checks take the uids as arguments and the test passes the
    /// wrong one for each. Mutants: the file's owner check removed -> `Owner` is not returned; the
    /// directory's removed -> `Parent` is not returned.
    #[test]
    fn the_owner_of_the_api_file_and_of_its_directory_is_checked() {
        let s = scratch();
        write_api(&s, &api_text(), 0o600);
        let mine = std::fs::metadata(&s.root).expect("meta").uid();
        assert_eq!(
            read_api_file_as(&s.root, mine, mine).map(|p| p.is_some()),
            Ok(true)
        );
        assert_eq!(
            read_api_file_as(&s.root, mine, mine + 1).map(|p| p.is_some()),
            Err(ApiFileError::Owner),
            "a file of another owner was read"
        );
        assert_eq!(
            read_api_file_as(&s.root, mine + 1, mine).map(|p| p.is_some()),
            Err(ApiFileError::Parent),
            "a directory of another owner was trusted"
        );
    }

    /// A FIFO at `path` (mode 0600), made with `mkfifo`.
    fn make_fifo(path: &Path) {
        use std::os::unix::ffi::OsStrExt as _;
        let name = std::ffi::CString::new(path.as_os_str().as_bytes()).expect("no NUL in a path");
        // SAFETY: `name` is a valid NUL-terminated path that outlives the call.
        let rc = unsafe { libc::mkfifo(name.as_ptr(), 0o600) };
        assert_eq!(rc, 0, "mkfifo: {}", std::io::Error::last_os_error());
        // (the umask may have taken bits off)
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    }

    /// What a start does with something that is not a file at the fixed name (`make` puts it
    /// there): `NotAFile`, NO pair and ONE warning in the problem's fixed words, and the start does
    /// not hang. The reads run on a thread the test waits for with a bound: a FIFO opened without
    /// `O_NONBLOCK` waits for a writer that never comes.
    fn no_pair_at_the_fixed_name(kind: &str, make: impl FnOnce(&Path)) {
        let s = scratch();
        let path = s.root.join(API_FILE);
        make(&path);
        let (data, profile, root) = (s.data.clone(), s.profile.clone(), s.root.clone());
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let env = env_of(vec![
                ("XDG_DATA_HOME", data.into_os_string()),
                (HOME_ENV, profile.into_os_string()),
            ]);
            let read = read_api_file(&root).map(|p| p.is_some());
            let (pair, events) = events_of(|| dev_api(true, &env).is_some());
            let _ = sender.send((read, pair, events));
        });
        let answer = receiver.recv_timeout(std::time::Duration::from_secs(10));
        if matches!(answer, Err(std::sync::mpsc::RecvTimeoutError::Timeout)) {
            // Let a reader stuck in `open` go (a writer arrives), so no thread is left behind.
            let _ = std::fs::OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&path);
        }
        let (read, pair, events) = answer.unwrap_or_else(|e| {
            panic!("{kind}: the read of the API file did not come back ({e}): the start would hang")
        });
        assert_eq!(read, Err(ApiFileError::NotAFile), "{kind}");
        assert!(!pair, "{kind}: a pair came out of it");
        assert_eq!(events.len(), 1, "{kind}: one warning is due: {events:?}");
        assert_eq!(events[0].0, tracing::Level::WARN, "{kind}");
        assert!(
            events[0].1.contains(&ApiFileError::NotAFile.to_string()),
            "{kind}: {events:?}"
        );
    }

    /// A directory at the fixed name. Mutant: the regular-file check of `read_api_file_as` removed
    /// -> the directory (made with the default mode) is `Mode`, not `NotAFile`.
    #[test]
    fn a_directory_at_the_fixed_name_is_no_api_pair() {
        no_pair_at_the_fixed_name("directory", |path| {
            std::fs::create_dir(path).expect("dir");
        });
    }

    /// A FIFO (mode 0600, no content) at the fixed name. Mutants: the regular-file check removed
    /// -> it is `Form` (an empty file); `O_NONBLOCK` removed -> the `open` never returns (the
    /// test's bound ends it after 10 s).
    #[test]
    fn a_fifo_at_the_fixed_name_is_no_api_pair_and_does_not_hang_the_start() {
        no_pair_at_the_fixed_name("fifo", make_fifo);
    }

    /// H3: a login needs the pair first, then both helper files.
    /// Mutant: `parts_state` ignores the files -> the missing-library cases are `Ready`.
    #[test]
    fn the_parts_state_needs_a_pair_and_both_files() {
        let tmp = tempfile::tempdir().expect("tmp");
        let helper = tmp.path().join("kourier-tdlib");
        let tdjson = tmp.path().join("libtdjson.so");
        let files = HelperFiles {
            helper: helper.clone(),
            tdjson: tdjson.clone(),
            no_dump: false,
        };
        let missing = PartsState::Missing;
        assert_eq!(parts_state(false, &files), missing(TelegramMissing::ApiKey));
        assert_eq!(parts_state(true, &files), missing(TelegramMissing::Library));
        std::fs::write(&helper, b"").expect("helper");
        assert_eq!(parts_state(true, &files), missing(TelegramMissing::Library));
        std::fs::write(&tdjson, b"").expect("library");
        assert_eq!(parts_state(true, &files), PartsState::Ready);
        assert_eq!(parts_state(false, &files), missing(TelegramMissing::ApiKey));
        assert_eq!(
            parts_state(true, &HelperFiles::not_found()),
            missing(TelegramMissing::Library)
        );
    }
}

/// TR R0a: the dial confirmation (A1, A2). Only a debug build has the reader, so only a debug
/// build has these tests. No test names the dialing mode's variant: they read `DialMode::word`.
#[cfg(all(test, debug_assertions))]
mod dial_tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;
    use crate::process::EXPECT_NUMBER_TAIL_ENV;

    /// The clock of every case.
    const NOW: u64 = 1_800_000_000_000;

    /// A scratch keeper root `data/kourier-tg-run` (0700) with the profile `p` below it and the
    /// profile's `tg-run` (0700): where the run tool writes.
    struct Run {
        _tmp: tempfile::TempDir,
        data: PathBuf,
        root: PathBuf,
        profile: PathBuf,
        dir: PathBuf,
    }

    fn run() -> Run {
        let tmp = tempfile::tempdir().expect("tmp");
        let base = std::fs::canonicalize(tmp.path()).expect("canonical");
        let data = base.join("data");
        let root = data.join(RUN_DIR);
        let profile = root.join("p");
        let dir = profile.join(DIAL_DIR);
        std::fs::create_dir_all(&dir).expect("dirs");
        for private in [&root, &dir] {
            std::fs::set_permissions(private, std::fs::Permissions::from_mode(0o700))
                .expect("chmod");
        }
        Run {
            _tmp: tmp,
            data,
            root,
            profile,
            dir,
        }
    }

    /// The nonce the tool would hand the app: 64 hex digits, built at run time.
    fn nonce() -> String {
        "ab".repeat(32)
    }

    /// The sentinel tail of the run tool: 15 digits.
    fn tail() -> OsString {
        "0".repeat(15).into()
    }

    /// The file the run tool writes (text of one JSON object).
    fn dial_json(issued_ms: u64, env: &str, nonce: &str) -> String {
        format!(
            r#"{{"version":1,"issued_ms":{issued_ms},"env":"{env}","nonce_sha256":"{}"}}"#,
            sha256_hex(nonce.as_bytes())
        )
    }

    /// Writes `text` as the confirmation file of `dir` with `mode`.
    fn write_in(dir: &Path, text: &str, mode: u32) {
        let path = dir.join(DIAL_FILE);
        let _ = std::fs::remove_file(&path);
        std::fs::write(&path, text).expect("write");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).expect("chmod");
    }

    /// The good file of the run tool, fresh at `NOW`.
    fn write_good(run: &Run) {
        write_in(&run.dir, &dial_json(NOW, "test", &nonce()), 0o600);
    }

    fn file_of(run: &Run) -> PathBuf {
        run.dir.join(DIAL_FILE)
    }

    /// An environment that can be edited, and handed on as a closure.
    #[derive(Clone)]
    struct Vars(Vec<(&'static str, OsString)>);

    impl Vars {
        /// What the run tool's app command carries, in `run`.
        fn good(run: &Run) -> Self {
            Self(vec![
                ("XDG_DATA_HOME", run.data.clone().into_os_string()),
                (HOME_ENV, run.profile.clone().into_os_string()),
                (DIAL_ENV, nonce().into()),
                (TELEGRAM_ENV, "test".into()),
                (EXPECT_NUMBER_TAIL_ENV, tail()),
            ])
        }

        fn set(&mut self, name: &'static str, value: impl Into<OsString>) {
            self.remove(name);
            self.0.push((name, value.into()));
        }

        fn remove(&mut self, name: &str) {
            self.0.retain(|(n, _)| *n != name);
        }

        fn env(&self) -> impl Fn(&str) -> Option<OsString> + Send + 'static {
            let vars = self.0.clone();
            move |k| vars.iter().find(|(n, _)| *n == k).map(|(_, v)| v.clone())
        }
    }

    /// The decision as its two words.
    fn words(d: DialDecision) -> (&'static str, &'static str) {
        (d.mode.word(), d.reason)
    }

    fn decide(vars: &Vars) -> (&'static str, &'static str) {
        words(dial_confirmation(true, &vars.env(), NOW))
    }

    const REFUSED: &str = "refusing";
    const DIALS: (&str, &str) = ("telegram", CONFIRMED);

    /// A FIFO at `path` (mode 0600), made with `mkfifo`.
    fn make_fifo(path: &Path) {
        use std::os::unix::ffi::OsStrExt as _;
        let name = std::ffi::CString::new(path.as_os_str().as_bytes()).expect("no NUL in a path");
        // SAFETY: `name` is a valid NUL-terminated path that outlives the call.
        let rc = unsafe { libc::mkfifo(name.as_ptr(), 0o600) };
        assert_eq!(rc, 0, "mkfifo: {}", std::io::Error::last_os_error());
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    }

    /// A1, the whole table of D-10. Every row starts from the good run (a fresh file whose hash is
    /// the variable's, a run profile, the test servers, the sentinel tail) and changes ONE thing.
    /// Mutants: (a) `if !debug` -> `if false` (the release row); (b) the window check removed (the
    /// two stale rows); (c) the hash compare removed (the mismatch rows); (d) the unlink removed (the
    /// second read); (e) `run_profile(env)?` -> `keeper_root(env)?` (the rows with a profile that
    /// is not a direct child of the keeper root, each with a good file in it); the `servers(..)` check or
    /// the file's `env` compare removed (the env rows); the tail check removed (the tail rows).
    #[test]
    fn the_dial_is_confirmed_only_by_a_fresh_file_and_its_nonce_in_a_run_profile() {
        type Tweak = Box<dyn Fn(&Run, &mut Vars)>;
        let file_with = |text: String, mode: u32| -> Tweak {
            Box::new(move |run, _| write_in(&run.dir, &text, mode))
        };
        let var = |name: &'static str, value: &'static str| -> Tweak {
            Box::new(move |_, vars| vars.set(name, value))
        };
        let without = |name: &'static str| -> Tweak { Box::new(move |_, vars| vars.remove(name)) };
        let good = dial_json(NOW, "test", &nonce());
        let other = "cd".repeat(32);
        let hash = sha256_hex(nonce().as_bytes());
        let json_with = |from: &str, to: &str| good.replace(from, to);
        let rows: Vec<(&str, Tweak, (&str, &str))> = vec![
            ("the good case", Box::new(|_, _| {}), DIALS),
            // The profile.
            (
                "no KOURIER_HOME",
                without(HOME_ENV),
                (REFUSED, Refusal::NoProfile.word()),
            ),
            (
                "the keeper root itself is the profile",
                Box::new(|run, vars| {
                    std::fs::create_dir_all(run.root.join(DIAL_DIR)).expect("dir");
                    std::fs::set_permissions(
                        run.root.join(DIAL_DIR),
                        std::fs::Permissions::from_mode(0o700),
                    )
                    .expect("chmod");
                    write_in(
                        &run.root.join(DIAL_DIR),
                        &dial_json(NOW, "test", &nonce()),
                        0o600,
                    );
                    vars.set(HOME_ENV, run.root.clone());
                }),
                (REFUSED, Refusal::NoProfile.word()),
            ),
            (
                "a profile outside the keeper root",
                Box::new(|run, vars| {
                    let elsewhere = run.data.join("other");
                    let dir = elsewhere.join(DIAL_DIR);
                    std::fs::create_dir_all(&dir).expect("dirs");
                    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
                        .expect("chmod");
                    write_in(&dir, &dial_json(NOW, "test", &nonce()), 0o600);
                    vars.set(HOME_ENV, elsewhere);
                }),
                (REFUSED, Refusal::NoProfile.word()),
            ),
            (
                "a profile two levels below the keeper root",
                Box::new(|run, vars| {
                    let deeper = run.profile.join("q");
                    let dir = deeper.join(DIAL_DIR);
                    std::fs::create_dir_all(&dir).expect("dirs");
                    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
                        .expect("chmod");
                    write_in(&dir, &dial_json(NOW, "test", &nonce()), 0o600);
                    vars.set(HOME_ENV, deeper);
                }),
                (REFUSED, Refusal::NoProfile.word()),
            ),
            // The variable and the file.
            (
                "no variable",
                without(DIAL_ENV),
                (REFUSED, Refusal::NoConfirmation.word()),
            ),
            (
                "the variable without a file",
                Box::new(|run, _| std::fs::remove_file(file_of(run)).expect("rm")),
                (REFUSED, Refusal::File.word()),
            ),
            (
                "the nonce is another one",
                Box::new(move |_, vars| vars.set(DIAL_ENV, other.clone())),
                (REFUSED, Refusal::Mismatch.word()),
            ),
            (
                "an empty variable",
                var(DIAL_ENV, ""),
                (REFUSED, Refusal::Mismatch.word()),
            ),
            (
                "the file holds the nonce itself, not its hash",
                file_with(json_with(&hash, &nonce()), 0o600),
                (REFUSED, Refusal::Mismatch.word()),
            ),
            // The window.
            (
                "issued 121 s ago",
                file_with(dial_json(NOW - 121_000, "test", &nonce()), 0o600),
                (REFUSED, Refusal::Stale.word()),
            ),
            (
                "issued 6 s ahead",
                file_with(dial_json(NOW + 6_000, "test", &nonce()), 0o600),
                (REFUSED, Refusal::Stale.word()),
            ),
            (
                "issued 120 s ago",
                file_with(dial_json(NOW - 120_000, "test", &nonce()), 0o600),
                DIALS,
            ),
            (
                "issued 5 s ahead",
                file_with(dial_json(NOW + 5_000, "test", &nonce()), 0o600),
                DIALS,
            ),
            // The file's own checks.
            (
                "mode 0644",
                file_with(good.clone(), 0o644),
                (REFUSED, "file"),
            ),
            (
                "mode 0640",
                file_with(good.clone(), 0o640),
                (REFUSED, "file"),
            ),
            (
                "mode 0660",
                file_with(good.clone(), 0o660),
                (REFUSED, "file"),
            ),
            (
                "mode 0400",
                file_with(good.clone(), 0o400),
                (REFUSED, "file"),
            ),
            (
                "a link to a good file",
                Box::new(|run, _| {
                    let real = run.data.join("real.json");
                    std::fs::write(&real, dial_json(NOW, "test", &nonce())).expect("write");
                    std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o600))
                        .expect("chmod");
                    std::fs::remove_file(file_of(run)).expect("rm");
                    std::os::unix::fs::symlink(&real, file_of(run)).expect("symlink");
                }),
                (REFUSED, "file"),
            ),
            (
                "a hard link to a good file",
                Box::new(|run, _| {
                    hard_link_to_a_good_file(run);
                }),
                (REFUSED, "file"),
            ),
            (
                "the directory is 0755",
                Box::new(|run, _| {
                    std::fs::set_permissions(&run.dir, std::fs::Permissions::from_mode(0o755))
                        .expect("chmod");
                }),
                (REFUSED, "file"),
            ),
            (
                "the directory is a link",
                Box::new(|run, _| {
                    let moved = run.profile.join("moved");
                    std::fs::rename(&run.dir, &moved).expect("rename");
                    std::os::unix::fs::symlink(&moved, &run.dir).expect("symlink");
                }),
                (REFUSED, "file"),
            ),
            (
                "no directory",
                Box::new(|run, _| std::fs::remove_dir_all(&run.dir).expect("rm")),
                (REFUSED, "file"),
            ),
            (
                "a directory at the file's name",
                Box::new(|run, _| {
                    std::fs::remove_file(file_of(run)).expect("rm");
                    std::fs::create_dir(file_of(run)).expect("dir");
                }),
                (REFUSED, "file"),
            ),
            (
                "bigger than 1 KiB",
                file_with(format!("{good}{}", " ".repeat(1100)), 0o600),
                (REFUSED, "file"),
            ),
            (
                "not JSON",
                file_with("go".to_owned(), 0o600),
                (REFUSED, "file"),
            ),
            ("empty", file_with(String::new(), 0o600), (REFUSED, "file")),
            (
                "another version",
                file_with(json_with("\"version\":1", "\"version\":2"), 0o600),
                (REFUSED, "file"),
            ),
            (
                "one field too many",
                file_with(json_with("\"version\":1", "\"version\":1,\"x\":1"), 0o600),
                (REFUSED, "file"),
            ),
            (
                "a field missing",
                file_with(json_with("\"env\":\"test\",", ""), 0o600),
                (REFUSED, "file"),
            ),
            (
                "a hash that is short",
                file_with(json_with(&hash, &hash[1..]), 0o600),
                (REFUSED, "file"),
            ),
            (
                "a hash in upper case",
                file_with(json_with(&hash, &hash.to_uppercase()), 0o600),
                (REFUSED, "file"),
            ),
            // The servers.
            (
                "the env switch is unset",
                without(TELEGRAM_ENV),
                (REFUSED, Refusal::Env.word()),
            ),
            (
                "the env switch says production",
                var(TELEGRAM_ENV, "production"),
                (REFUSED, Refusal::Env.word()),
            ),
            (
                "the env switch is not valid",
                var(TELEGRAM_ENV, "staging"),
                (REFUSED, Refusal::Env.word()),
            ),
            (
                "the file says production",
                file_with(dial_json(NOW, "production", &nonce()), 0o600),
                (REFUSED, Refusal::Env.word()),
            ),
            (
                "the file says Test",
                file_with(dial_json(NOW, "Test", &nonce()), 0o600),
                (REFUSED, Refusal::Env.word()),
            ),
            (
                "the file says nothing for env",
                file_with(dial_json(NOW, "", &nonce()), 0o600),
                (REFUSED, Refusal::Env.word()),
            ),
            // The tail.
            (
                "the tail is absent",
                without(EXPECT_NUMBER_TAIL_ENV),
                (REFUSED, Refusal::NoTail.word()),
            ),
            (
                "the tail is empty",
                var(EXPECT_NUMBER_TAIL_ENV, ""),
                (REFUSED, Refusal::NoTail.word()),
            ),
            (
                "the tail is blank",
                var(EXPECT_NUMBER_TAIL_ENV, "  "),
                (REFUSED, Refusal::NoTail.word()),
            ),
            (
                "the tail is not digits",
                var(EXPECT_NUMBER_TAIL_ENV, "12ab"),
                (REFUSED, Refusal::NoTail.word()),
            ),
        ];
        for (name, tweak, expected) in rows {
            let run = run();
            write_good(&run);
            let mut vars = Vars::good(&run);
            tweak(&run, &mut vars);
            assert_eq!(decide(&vars), expected, "{name}");
        }

        // (a) A release build, however good the run: nothing is read, the file stays.
        let run = run();
        write_good(&run);
        let vars = Vars::good(&run);
        assert_eq!(
            words(dial_confirmation(false, &vars.env(), NOW)),
            (REFUSED, Refusal::Release.word()),
            "a release build"
        );
        assert!(file_of(&run).exists(), "a release build consumed the file");
        // (d) A second read: the first took the file.
        assert_eq!(decide(&vars), DIALS, "the first read");
        assert!(!file_of(&run).exists(), "the file is still there");
        assert_eq!(
            decide(&vars),
            (REFUSED, Refusal::File.word()),
            "a second read"
        );
    }

    /// The confirmation file's name made a hard link to a good 0600 file of the profile (the other
    /// name is `other.json` in the profile directory, returned). Mutant for the row: the
    /// `st_nlink` check removed.
    fn hard_link_to_a_good_file(run: &Run) -> PathBuf {
        let other = run.profile.join("other.json");
        std::fs::write(&other, dial_json(NOW, "test", &nonce())).expect("write");
        std::fs::set_permissions(&other, std::fs::Permissions::from_mode(0o600)).expect("chmod");
        std::fs::remove_file(file_of(run)).expect("rm");
        std::fs::hard_link(&other, file_of(run)).expect("link");
        other
    }

    /// A1, TR R0a fix G-6 (review F6): a confirmation file that had ANOTHER name (a hard link) is
    /// refused as `file`, and the other name is left untouched (the run tool's file is gone: it
    /// was taken like any other). Without the other name the very same bytes confirm. Mutant: the
    /// `st_nlink == 0` check removed -> the first assertion says `confirmed`.
    #[test]
    fn a_dial_file_that_had_another_name_is_refused_and_the_other_name_is_left() {
        let run = run();
        write_good(&run);
        let other = hard_link_to_a_good_file(&run);
        let before = std::fs::read(&other).expect("read");
        assert_eq!(
            decide(&Vars::good(&run)),
            (REFUSED, Refusal::File.word()),
            "a file with another name"
        );
        assert!(!file_of(&run).exists(), "the linked name was left behind");
        assert_eq!(
            std::fs::read(&other).expect("the other name survives"),
            before,
            "the other name changed"
        );
        // The control: the same bytes under their only name confirm.
        write_good(&run);
        assert_eq!(decide(&Vars::good(&run)), DIALS, "the control");
    }

    /// A1, the owners: the file's and its directory's. A plain user has no second uid, so the
    /// judgment takes both as arguments and the test passes the wrong one for each, with the right
    /// pair as the control. Mutants: the file's owner check removed; the directory's removed.
    #[test]
    fn the_owner_of_the_dial_file_and_of_its_directory_is_checked() {
        use std::os::unix::fs::MetadataExt as _;

        let run = run();
        let mine = std::fs::metadata(&run.dir).expect("meta").uid();
        let env = Vars::good(&run).env();
        let try_as = |dir_uid: u32, file_uid: u32| {
            write_good(&run);
            dial_confirmation_as(true, &env, NOW, dir_uid, file_uid)
        };
        assert_eq!(try_as(mine, mine), Ok(()), "the control");
        assert_eq!(
            try_as(mine, mine + 1),
            Err(Refusal::File),
            "a file of another owner"
        );
        assert_eq!(
            try_as(mine + 1, mine),
            Err(Refusal::File),
            "a directory of another owner"
        );
    }

    /// A1, the FIFO at the file's name: the read must not wait for a writer (`O_NONBLOCK`), so it
    /// runs on a thread the test waits for 10 s. Mutant: `O_NONBLOCK` removed -> the `open` never
    /// returns (the bound ends it).
    #[test]
    fn a_fifo_at_the_dial_file_is_refused_and_does_not_hang_the_start() {
        let run = run();
        make_fifo(&file_of(&run));
        let vars = Vars::good(&run);
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = sender.send(words(dial_confirmation(true, &vars.env(), NOW)));
        });
        let answer = receiver.recv_timeout(std::time::Duration::from_secs(10));
        if matches!(answer, Err(std::sync::mpsc::RecvTimeoutError::Timeout)) {
            // Let a reader stuck in `open` go (a writer arrives), so no thread is left behind.
            let _ = std::fs::OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(file_of(&run));
        }
        let answer = answer.unwrap_or_else(|e| {
            panic!("the read of the dial file did not come back ({e}): the start would hang")
        });
        assert_eq!(answer, (REFUSED, Refusal::File.word()));
        assert!(!file_of(&run).exists(), "the FIFO is still there");
    }

    /// A2: a start WITHOUT the variable (a plain debug start, `--print-networks`) leaves the file
    /// alone; a start WITH it consumes the file whatever the verdict (the profile, the window, the
    /// hash, the servers, the tail). Mutant: the file unlinked before the variable is looked at ->
    /// the first assertion fails.
    #[test]
    fn a_start_without_the_variable_leaves_the_file_alone() {
        let run = run();
        write_good(&run);
        let mut vars = Vars::good(&run);
        vars.remove(DIAL_ENV);
        for _ in 0..2 {
            assert_eq!(decide(&vars), (REFUSED, Refusal::NoConfirmation.word()));
            assert!(
                file_of(&run).exists(),
                "a start without the variable took the file"
            );
        }
        // The file is whole: the same file confirms the start that carries the variable.
        let with = Vars::good(&run);
        assert_eq!(decide(&with), DIALS);

        // With the variable, the file is gone after every verdict that read it.
        type Tweak = Box<dyn Fn(&mut Vars)>;
        let tweaks: Vec<(&str, Tweak)> = vec![
            (
                "the servers",
                Box::new(|v| v.set(TELEGRAM_ENV, "production")),
            ),
            ("the tail", Box::new(|v| v.remove(EXPECT_NUMBER_TAIL_ENV))),
            ("the nonce", Box::new(|v| v.set(DIAL_ENV, "cd".repeat(32)))),
        ];
        for (name, tweak) in tweaks {
            write_good(&run);
            let mut vars = Vars::good(&run);
            tweak(&mut vars);
            assert_ne!(decide(&vars).0, "telegram", "{name}");
            assert!(!file_of(&run).exists(), "{name}: the file was left behind");
        }
        write_in(&run.dir, &dial_json(NOW - 121_000, "test", &nonce()), 0o600);
        assert_eq!(decide(&Vars::good(&run)).1, Refusal::Stale.word());
        assert!(!file_of(&run).exists(), "a stale file was left behind");
        write_in(&run.dir, &dial_json(NOW, "test", &nonce()), 0o644);
        assert_eq!(decide(&Vars::good(&run)).1, Refusal::File.word());
        assert!(
            !file_of(&run).exists(),
            "a file of the wrong mode was left behind"
        );
        // Outside a run profile nothing is read either: the file stays.
        write_good(&run);
        let mut outside = Vars::good(&run);
        outside.remove(HOME_ENV);
        assert_eq!(decide(&outside).1, Refusal::NoProfile.word());
        assert!(
            file_of(&run).exists(),
            "a start outside the profile took the file"
        );
    }

    /// The INFO line's words are fixed and carry no value (the hash, the nonce, the path).
    #[test]
    fn the_words_of_the_dialer_line_are_fixed() {
        let all = [
            Refusal::Release,
            Refusal::NoProfile,
            Refusal::NoConfirmation,
            Refusal::File,
            Refusal::Stale,
            Refusal::Mismatch,
            Refusal::Env,
            Refusal::NoTail,
        ];
        let words: Vec<_> = all.iter().map(|r| r.word()).collect();
        assert_eq!(
            words,
            [
                "release",
                "no-profile",
                "no-confirmation",
                "file",
                "stale",
                "mismatch",
                "env",
                "no-tail"
            ]
        );
        assert_eq!(CONFIRMED, "confirmed");
        assert_eq!(DialMode::Refuse.word(), "refusing");
    }
}
