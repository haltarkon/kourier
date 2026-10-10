//! Logging (§9.1): `tracing` to stderr and to a rotating file `logs/kourier.log` in the state
//! directory (7 files × 10 MiB), filtered by `KOURIER_LOG` (default `info`), with the `log`
//! bridge for crates that use `log` (libsignal).
//!
//! What may be logged is a property of the call sites, not of this module: ids in their log
//! forms (`aci:…1f2e`, `conv#a1b2`), counts, kinds and states, never message bodies, names,
//! numbers, keys, tokens or QR contents. The formatter prints no span fields beyond what the
//! events carry and never the thread name (it can carry an account uuid prefix only).
//!
//! Kourier's own call sites follow that; libsignal's do not always (its protocol crate prints
//! a peer's full `<service id>.<device>` on some WARN and ERROR paths, and at INFO on every
//! decrypted prekey message). So every line also passes [`redact`] on its way to stderr and to
//! the file: whole UUIDs keep their last four digits (`…1f2e`, the form Kourier's own ids
//! have), phone numbers and `sgnl://` / `signalcaptcha://` links are cut out. It is a net
//! under the call sites, not a licence for them.

use std::borrow::Cow;
use std::fs::{File, OpenOptions};
use std::io::{self, IsTerminal, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer as _};

/// The default log filter. libsignal-protocol logs full service ids and key fingerprints at
/// INFO (e.g. "decrypted PreKey message from <uuid>.1"), which §9.1 forbids, so it is capped at
/// WARN unless the user sets [`LOG_ENV`] explicitly.
pub const DEFAULT_FILTER: &str = "info,libsignal_protocol=warn,libsignal_message_backup=error";

/// The environment variable with the log filter (`tracing` `EnvFilter` syntax).
pub const LOG_ENV: &str = "KOURIER_LOG";
/// The file name of the current log.
pub const LOG_FILE: &str = "kourier.log";
/// Rotate when the current file would exceed this size.
pub const MAX_FILE_BYTES: u64 = 10 * 1024 * 1024;
/// Files kept, the current one included (`kourier.log`, `kourier.log.1` … `.6`).
pub const KEPT_FILES: usize = 7;

/// A log file that rotates by size. Clones share the file.
#[derive(Clone, Debug)]
pub struct RotatingFile {
    inner: Arc<Mutex<FileState>>,
}

#[derive(Debug)]
struct FileState {
    dir: PathBuf,
    max_bytes: u64,
    kept: usize,
    file: Option<File>,
    size: u64,
}

fn open_log(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
}

impl RotatingFile {
    /// Opens (appends to) `dir/kourier.log`, rotating at `max_bytes` and keeping `kept` files.
    pub fn open(dir: &Path, max_bytes: u64, kept: usize) -> io::Result<Self> {
        crate::paths::create_private_dir(dir)?;
        let file = open_log(&dir.join(LOG_FILE))?;
        let size = file.metadata()?.len();
        Ok(Self {
            inner: Arc::new(Mutex::new(FileState {
                dir: dir.to_owned(),
                max_bytes,
                kept: kept.max(1),
                file: Some(file),
                size,
            })),
        })
    }
}

impl FileState {
    fn name(&self, n: usize) -> PathBuf {
        if n == 0 {
            self.dir.join(LOG_FILE)
        } else {
            self.dir.join(format!("{LOG_FILE}.{n}"))
        }
    }

    fn rotate(&mut self) -> io::Result<()> {
        self.file = None;
        let oldest = self.name(self.kept - 1);
        match std::fs::remove_file(&oldest) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
        for n in (0..self.kept - 1).rev() {
            match std::fs::rename(self.name(n), self.name(n + 1)) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
                _ => {}
            }
        }
        self.file = Some(open_log(&self.name(0))?);
        self.size = 0;
        Ok(())
    }

    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let len = u64::try_from(buf.len()).unwrap_or(u64::MAX);
        if self.size > 0 && self.size.saturating_add(len) > self.max_bytes {
            self.rotate()?;
        }
        if self.file.is_none() {
            self.file = Some(open_log(&self.name(0))?);
        }
        let Some(file) = self.file.as_mut() else {
            return Ok(buf.len());
        };
        file.write_all(buf)?;
        self.size = self.size.saturating_add(len);
        Ok(buf.len())
    }
}

/// Where a UUID (`8-4-4-4-12` hex digits) at the start of `b` ends, if there is one.
fn uuid_len(b: &[u8]) -> Option<usize> {
    const LEN: usize = 36;
    let candidate = b.get(..LEN)?;
    let shaped = candidate.iter().enumerate().all(|(i, c)| match i {
        8 | 13 | 18 | 23 => *c == b'-',
        _ => c.is_ascii_hexdigit(),
    });
    // Not the head of a longer run of hex digits (a hash that happens to have dashes).
    let ends = b.get(LEN).is_none_or(|c| !c.is_ascii_hexdigit());
    (shaped && ends).then_some(LEN)
}

/// Where a phone number in E.164 form (`+` and 7 to 15 digits) at the start of `b` ends.
fn e164_len(b: &[u8]) -> Option<usize> {
    if b.first() != Some(&b'+') {
        return None;
    }
    let digits = b[1..].iter().take_while(|c| c.is_ascii_digit()).count();
    (7..=15).contains(&digits).then_some(1 + digits)
}

/// Where a link that carries a secret (`sgnl://…` with a linking key, `signalcaptcha://…`
/// with a challenge solution) at the start of `b` ends: at the next blank, quote or bracket.
fn secret_link_len(b: &[u8]) -> Option<(usize, &'static str)> {
    for scheme in ["sgnl://", "signalcaptcha://"] {
        if b.starts_with(scheme.as_bytes()) {
            let end = b
                .iter()
                .position(|c| {
                    c.is_ascii_whitespace() || matches!(c, b'"' | b'\'' | b')' | b']' | b'>')
                })
                .unwrap_or(b.len());
            // The bare scheme is a word of a message ("sgnl:// link: …"), not a link.
            return (end > scheme.len()).then_some((end, scheme));
        }
    }
    None
}

/// Takes out of one log line what §9.1 never allows in a log, whoever wrote the line: a whole
/// UUID becomes `…` and its last four digits (service ids, group and account ids: the short
/// form still tells two apart), a phone number becomes `+[number]`, a `sgnl://` or
/// `signalcaptcha://` link keeps only its scheme. Text without any of these is returned as
/// it is.
pub fn redact(line: &str) -> Cow<'_, str> {
    let b = line.as_bytes();
    let mut out: Option<String> = None;
    let mut copied = 0;
    let mut i = 0;
    while i < b.len() {
        // Only ASCII starts a match, and a match is ASCII: cuts fall on character borders.
        let before_ok = i == 0 || !b[i - 1].is_ascii_alphanumeric();
        let hit: Option<(usize, Cow<'static, str>)> = match b[i] {
            c if c.is_ascii_hexdigit() && (i == 0 || !b[i - 1].is_ascii_hexdigit()) => {
                uuid_len(&b[i..]).map(|n| (n, Cow::Owned(format!("…{}", &line[i + n - 4..i + n]))))
            }
            b'+' if before_ok => e164_len(&b[i..]).map(|n| (n, Cow::Borrowed("+[number]"))),
            b's' if before_ok => secret_link_len(&b[i..])
                .map(|(n, scheme)| (n, Cow::Owned(format!("{scheme}[redacted]")))),
            _ => None,
        };
        match hit {
            Some((n, replacement)) => {
                let out = out.get_or_insert_with(|| String::with_capacity(line.len()));
                out.push_str(&line[copied..i]);
                out.push_str(&replacement);
                i += n;
                copied = i;
            }
            None => i += 1,
        }
    }
    match out {
        Some(mut out) => {
            out.push_str(&line[copied..]);
            Cow::Owned(out)
        }
        None => Cow::Borrowed(line),
    }
}

/// A log writer that passes every formatted event through [`redact`]. The `fmt` layer hands
/// each event over in one `write_all`, so a pattern is never split over two writes.
#[derive(Debug)]
pub struct Redacting<W>(W);

impl<W: Write> Write for Redacting<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match std::str::from_utf8(buf) {
            Ok(text) => self.0.write_all(redact(text).as_bytes())?,
            // Not text (never from the formatter): through as it is.
            Err(_) => self.0.write_all(buf)?,
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

/// stderr behind [`redact`].
fn redacted_stderr() -> Redacting<io::Stderr> {
    Redacting(io::stderr())
}

/// One write through a [`RotatingFile`].
#[derive(Debug)]
pub struct RotatingWriter<'a>(&'a RotatingFile);

impl Write for RotatingWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0
            .inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut state = self.0.inner.lock().unwrap_or_else(PoisonError::into_inner);
        match state.file.as_mut() {
            Some(f) => f.flush(),
            None => Ok(()),
        }
    }
}

impl<'a> MakeWriter<'a> for RotatingFile {
    type Writer = Redacting<RotatingWriter<'a>>;

    fn make_writer(&'a self) -> Self::Writer {
        Redacting(RotatingWriter(self))
    }
}

/// Installs the global subscriber: stderr, plus `log_dir/kourier.log` when given. Returns
/// `false` when a subscriber was already installed (tests that start several apps) or the log
/// file could not be opened (then stderr only, with a warning).
pub fn init(log_dir: Option<&Path>) -> bool {
    let filter =
        || EnvFilter::try_from_env(LOG_ENV).unwrap_or_else(|_| EnvFilter::new(DEFAULT_FILTER));
    let stderr = tracing_subscriber::fmt::layer()
        .with_writer(redacted_stderr)
        .with_ansi(io::stderr().is_terminal())
        .with_thread_names(false)
        .with_filter(filter());
    let mut file_error = None;
    let file = log_dir.and_then(
        |dir| match RotatingFile::open(dir, MAX_FILE_BYTES, KEPT_FILES) {
            Ok(f) => Some(f),
            Err(e) => {
                file_error = Some(e);
                None
            }
        },
    );
    let file_layer = file.map(|f| {
        tracing_subscriber::fmt::layer()
            .with_writer(f)
            .with_ansi(false)
            .with_thread_names(false)
            .with_filter(filter())
    });
    let installed = tracing_subscriber::registry()
        .with(stderr)
        .with(file_layer)
        .try_init()
        .is_ok();
    if let Some(e) = file_error {
        tracing::warn!(error = %e, "cannot open the log file; logging to stderr only");
    }
    installed
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lines as libsignal's protocol crate writes them (a peer's full address) and what else
    /// must never reach a log, whoever formats it.
    #[test]
    fn full_ids_numbers_and_secret_links_are_cut_out() {
        assert_eq!(
            redact(
                "WARN libsignal_protocol: no session with 9d0652a3-dcc3-4d11-975f-74d61598733f.2 (device 2)"
            ),
            "WARN libsignal_protocol: no session with …733f.2 (device 2)"
        );
        assert_eq!(
            redact(
                "sender PNI:01234567-89ab-cdef-0123-456789ABCDEF, again 01234567-89ab-cdef-0123-456789abcdef"
            ),
            "sender PNI:…CDEF, again …cdef"
        );
        assert_eq!(
            redact("number=+14155550101 e164=\"+4915123456789\""),
            "number=+[number] e164=\"+[number]\""
        );
        assert_eq!(
            redact(
                "qr sgnl://linkdevice?uuid=abc&pub_key=BQ%2Fxyz shown; signalcaptcha://signal-hcaptcha.5f.tok done"
            ),
            "qr sgnl://[redacted] shown; signalcaptcha://[redacted] done"
        );
        // A path with an account directory: the uuid goes, the rest stays.
        assert_eq!(
            redact("/home/u/kourier/data/accounts/9d0652a3-dcc3-4d11-975f-74d61598733f/backend.db"),
            "/home/u/kourier/data/accounts/…733f/backend.db"
        );
        // Text around multi-byte characters is kept as it is.
        assert_eq!(redact("größe … +14155550101 ✓"), "größe … +[number] ✓");
    }

    /// Kourier's own log forms and ordinary numbers pass untouched (and unallocated).
    #[test]
    fn log_safe_text_is_not_touched() {
        for line in [
            "INFO kourier_signal: account=aci:…1f2e conv#a1b2 device linked",
            "DEBUG kourier_signal::requests: via=\"chat\" method=PUT route=/v2/keys status=200 ms=41",
            "skew_ms=-3600000 retry_in_s=30 count=+5 size=1048576",
            // Too short or too long for a phone number, or glued to a word.
            "+123456 +1234567890123456 a+14155550101",
            // Hex that is not a UUID.
            "digest 9d0652a3dcc34d11975f74d61598733f 9d0652a3-dcc3-4d11",
            "9d0652a3-dcc3-4d11-975f-74d61598733f0",
            "a message about sgnl links, and signalcaptcha too",
            // The scheme alone, as a message names it, is no link.
            "INFO kourier_qt::desktop: sgnl:// link: showing the window links=1",
            "signalcaptcha:// \"sgnl://\"",
            "",
        ] {
            assert!(matches!(redact(line), Cow::Borrowed(_)), "{line}");
        }
    }

    #[test]
    fn the_log_file_gets_redacted_lines() {
        let tmp = tempfile::tempdir().expect("tmp");
        let f = RotatingFile::open(tmp.path(), MAX_FILE_BYTES, KEPT_FILES).expect("open");
        f.make_writer()
            .write_all(b"decrypted PreKey message from 9d0652a3-dcc3-4d11-975f-74d61598733f.1\n")
            .expect("write");
        assert_eq!(
            std::fs::read_to_string(tmp.path().join(LOG_FILE)).expect("log"),
            "decrypted PreKey message from …733f.1\n"
        );
    }

    #[test]
    fn rotates_by_size_and_keeps_n_files() {
        let tmp = tempfile::tempdir().expect("tmp");
        let f = RotatingFile::open(tmp.path(), 100, 3).expect("open");
        let line = [b'x'; 40];
        for _ in 0..10 {
            f.make_writer().write_all(&line).expect("write");
        }
        let mut names: Vec<String> = std::fs::read_dir(tmp.path())
            .expect("dir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["kourier.log", "kourier.log.1", "kourier.log.2"]);
        for n in &names {
            let meta = std::fs::metadata(tmp.path().join(n)).expect("meta");
            assert!(meta.len() <= 100, "{n} is {} bytes", meta.len());
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        }
    }
}
