//! Staging a `connect`: the build, the stamped copies of the app, its helper and the TDLib library, the
//! switch file that makes the app register Telegram and open its link page once, and the probe that
//! proves the staged tree is one the app can log in from.
//!
//! The staged app is ALWAYS the tool's own `cargo build -p kourier -p kourier-tdlib`, made right before
//! it is copied (B-1): `--no-build` reuses only the copies this profile already holds, whose sums are
//! checked against `stamp.json` and whose commit is HEAD, never a `target/debug/kourier` some other
//! cargo run wrote. A binary built with the `test-support` feature (a `--tests` or `--all-targets`
//! build) refuses every dial anyway (it fails safe), but the tool never stages one.
//!
//! The layout under `<profile>/tg-run/` is the one the app looks for (`kourier-app/src/app.rs`
//! `telegram_helper_files`): the helper BESIDE the app (`bin/kourier-tdlib`), the library at
//! `<exe dir>/../tdlib/<soname>` (`tdlib/libtdjson.so.<version>`).

use std::ffi::OsString;
use std::fs::OpenOptions;
use std::io::Read as _;
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

use super::app_names::{REHEARSAL_ENV, TELEGRAM_ENV};
use super::profile::{ProfileDir, private_dir, read_json, read_switches, write_private};
use super::report::redact;

/// The app and its helper, as the app finds them side by side.
pub(super) const APP_BIN: &str = "kourier";
pub(super) const HELPER_BIN: &str = "kourier-tdlib";
/// The modes of the staged files: the two programs are EXECUTE-ONLY (the kernel then starts them
/// non-dumpable by the file's mode, so no other process of the user can read `/proc/<pid>/environ` of
/// the app from its first instruction on, which holds the confirmation's nonce), the library is
/// read-only (`dlopen` needs to read it).
pub(super) const PROGRAM_MODE: u32 = 0o100;
pub(super) const LIBRARY_MODE: u32 = 0o400;
/// The tail the app gets: 15 zeros. A phone number is at most 15 digits and a country code never starts
/// with 0, so no number ends in this and the tail check after any scan fails (D-9; wa_run.rs:1432
/// `SENTINEL_TAIL`).
pub(super) const SENTINEL_TAIL: &str = "000000000000000";
/// The app's variable for that tail (wa_run.rs:1434 `TAIL_ENV`).
pub(super) const TAIL_ENV: &str = "KOURIER_DEV_EXPECT_NUMBER_TAIL";
/// The words of the rehearsal switch and of the servers.
const REHEARSAL_WORD: &str = "telegram";
const SERVERS_WORD: &str = "test";
/// A silent app finds no audio server (wa_run.rs:1441 `NO_AUDIO_SERVER`).
const NO_AUDIO_SERVER: &str = "unix:/nonexistent/kourier-tg-run-no-audio";
const NO_PIPEWIRE: &str = "kourier-tg-run-no-audio";
/// The probe is given this long (it prints a line and exits).
pub(super) const PROBE_BOUND: Duration = Duration::from_secs(60);
/// How often a bounded command is looked at.
pub(super) const PROBE_POLL: Duration = Duration::from_millis(5);

// ---- The clock ------------------------------------------------------------------------------------

/// A monotonic clock in milliseconds. Every wait of the walk and the probe is bounded on one. A wait
/// calls [`Clock::idle`] once for every poll that found nothing, so a test clock can count IDLE POLLS
/// instead of calls to `now_ms`: a bound then reads as that many polls of silence, and a child that is
/// slow under load (but answers) does not use it up.
pub(super) trait Clock {
    fn now_ms(&self) -> u64;
    /// One poll of a wait found nothing new and has waited its poll. The wall clock needs no note of it.
    fn idle(&self) {}
}

/// The wall clock of the real run.
pub(super) struct RealClock(pub std::time::Instant);

impl Clock for RealClock {
    fn now_ms(&self) -> u64 {
        u64::try_from(self.0.elapsed().as_millis()).unwrap_or(u64::MAX)
    }
}

/// How a bounded wait is paced: how long it may last on `clock`, and how often it looks.
pub(super) struct Wait<'a> {
    pub bound: Duration,
    pub poll: Duration,
    pub clock: &'a dyn Clock,
}

// ---- Where things are -----------------------------------------------------------------------------

impl ProfileDir {
    /// Where the stamped programs and their stamp are.
    pub fn bin_dir(&self) -> PathBuf {
        self.tool().join("bin")
    }
    pub fn app_copy(&self) -> PathBuf {
        self.bin_dir().join(APP_BIN)
    }
    pub fn helper_copy(&self) -> PathBuf {
        self.bin_dir().join(HELPER_BIN)
    }
    pub fn stamp_file(&self) -> PathBuf {
        self.bin_dir().join("stamp.json")
    }
    /// The library's directory: `<exe dir>/../tdlib`.
    pub fn tdlib_dir(&self) -> PathBuf {
        self.tool().join("tdlib")
    }
    /// The app's log (redacted), appended to by every walk.
    pub fn app_log(&self) -> PathBuf {
        self.tool().join("app.log")
    }
    pub fn report_file(&self) -> PathBuf {
        self.tool().join("report.md")
    }
    /// The one-shot confirmation the app takes (`kourier-app`: `DIAL_DIR`/`DIAL_FILE`).
    pub fn dial_file(&self) -> PathBuf {
        self.tool().join(super::app_names::DIAL_FILE)
    }
}

// ---- Hashes and checked copies ----------------------------------------------------------------------

/// The SHA-256 of a file, by `sha256sum` (wa_run.rs:1519 `sha256_of`; xtask has no hash crate).
pub(super) fn sha256_of(path: &Path) -> Result<String> {
    let out = Command::new("sha256sum")
        .arg("--")
        .arg(path)
        .output()
        .context("run sha256sum")?;
    if !out.status.success() {
        bail!("sha256sum failed on a staged file");
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let sum = text.split_whitespace().next().unwrap_or_default();
    if sum.len() != 64 || !sum.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("sha256sum printed no hash for a staged file");
    }
    Ok(sum.to_ascii_lowercase())
}

/// Copies `source` to `tmp` (mode 0600) and returns its hash, which is the hash of both: the bytes are
/// hashed before and after the copy, so a file that changed while it was copied (a build running
/// beside) is refused and `tmp` removed (wa_run.rs:1539 `copy_checked`).
fn copy_checked(
    source: &Path,
    tmp: &Path,
    hash: &dyn Fn(&Path) -> Result<String>,
) -> Result<String> {
    let before = hash(source)?;
    // A copy a crashed start left is execute-only: it cannot be opened for writing again.
    let _ = std::fs::remove_file(tmp);
    {
        let mut from =
            std::fs::File::open(source).context("open a build output (run without --no-build)")?;
        let mut to = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(tmp)
            .context("create a staged copy")?;
        std::io::copy(&mut from, &mut to).context("copy a build output")?;
        to.sync_all()?;
    }
    if hash(tmp)? != before {
        let _ = std::fs::remove_file(tmp);
        bail!(
            "a build output changed while it was copied (a build running beside?): nothing was stamped. Start again."
        );
    }
    Ok(before)
}

/// The sibling `<name>.new` of `dest` (a file name with dots in it keeps all of them).
fn sibling_new(dest: &Path) -> PathBuf {
    let name = dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    dest.with_file_name(format!("{name}.new"))
}

/// Copies `source` to `dest` with `mode`: through a checked copy and a rename, so a running app keeps
/// its old inode. Returns the sum.
fn stage_file(source: &Path, dest: &Path, mode: u32) -> Result<String> {
    let tmp = sibling_new(dest);
    let sum = copy_checked(source, &tmp, &sha256_of)?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))?;
    std::fs::rename(&tmp, dest).context("replace a staged copy")?;
    Ok(sum)
}

/// The sum of an execute-only file: the tool makes it readable for the moment of the hash and puts the
/// mode back whatever the hash says (the app is not running, nothing is in its environment, and the
/// profile's directory is 0700). The tool cannot read it otherwise, which is the file's point.
fn sha256_of_program(path: &Path) -> Result<String> {
    struct Restore<'a>(&'a Path);
    impl Drop for Restore<'_> {
        fn drop(&mut self) {
            let _ = std::fs::set_permissions(self.0, std::fs::Permissions::from_mode(PROGRAM_MODE));
        }
    }
    let _restore = Restore(path);
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o400))
        .context("read a staged program")?;
    sha256_of(path)
}

// ---- The stamp --------------------------------------------------------------------------------------

/// What the staged copies are and what they were made from: the commit (a real run's copies are made
/// from main's clean HEAD; the rehearsal's have none) and the sums of the files. The files are the
/// profile's own and hold the stamp's `version` 1.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Stamp {
    pub commit: Option<String>,
    pub app_sha256: String,
    pub helper_sha256: Option<String>,
    /// The library's file name (its soname) and sum.
    pub library: Option<(String, String)>,
}

fn is_sum(text: &str) -> bool {
    text.len() == 64
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// A library file name the app can find: `libtdjson.so.` and a version, nothing that is a path.
fn is_library_name(name: &str) -> bool {
    name.strip_prefix("libtdjson.so.").is_some_and(|v| {
        !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit() || b == b'.') && !v.contains("..")
    })
}

impl Stamp {
    fn to_json(&self, made_ms: u64) -> Value {
        json!({
            "version": 1,
            "commit": self.commit,
            "app_sha256": self.app_sha256,
            "helper_sha256": self.helper_sha256,
            "library_name": self.library.as_ref().map(|l| &l.0),
            "library_sha256": self.library.as_ref().map(|l| &l.1),
            "made_ms": made_ms,
        })
    }

    /// A stamp of this tool's shape or nothing: a real stamp (it has a commit) holds all three sums, a
    /// rehearsal's (none) holds the app's alone.
    fn from_json(v: &Value) -> Option<Self> {
        let text = |k: &str| v[k].as_str().map(str::to_owned);
        if v["version"] != json!(1) || v["made_ms"].as_u64().is_none() {
            return None;
        }
        let stamp = Self {
            commit: text("commit"),
            app_sha256: text("app_sha256").filter(|s| is_sum(s))?,
            helper_sha256: text("helper_sha256"),
            library: match (text("library_name"), text("library_sha256")) {
                (Some(name), Some(sum)) => Some((name, sum)),
                (None, None) => None,
                _ => return None,
            },
        };
        let real = stamp.commit.is_some();
        let good = stamp.helper_sha256.as_deref().is_none_or(is_sum)
            && stamp
                .library
                .as_ref()
                .is_none_or(|(n, s)| is_library_name(n) && is_sum(s))
            && stamp.helper_sha256.is_some() == real
            && stamp.library.is_some() == real;
        good.then_some(stamp)
    }
}

/// The TDLib library as the preflight checked it: its path and the sha256 of the bytes the checks read
/// (`tdlib::checked_library`). What a stage copies, or reuses, must have this sum.
#[derive(Clone, Debug)]
pub(super) struct CheckedLibrary {
    pub path: PathBuf,
    pub sha256: String,
}

/// The one sentence for a staged library that is not the checked one (H-3).
pub(super) const LIBRARY_NOT_CHECKED: &str =
    "the staged library is not the checked one: start again";

/// The build outputs a stage copies. A real stage has the helper and the library; the rehearsal's has
/// the app alone (the fake replaces the backend: no helper, no library).
pub(super) struct Sources<'a> {
    pub app: &'a Path,
    pub helper: Option<&'a Path>,
    pub library: Option<&'a CheckedLibrary>,
}

/// Makes the profile's staged copies and their stamp: the app and the helper EXECUTE-ONLY, the library
/// read-only, each hashed before and after its copy, the stamp (0600) last. No stamp stands while the
/// copies are replaced: a crash in between leaves copies `--no-build` refuses.
pub(super) fn stamp_copies(
    dir: &ProfileDir,
    sources: &Sources<'_>,
    commit: Option<&str>,
    now_ms: u64,
) -> Result<Stamp> {
    let real = commit.is_some();
    if sources.helper.is_some() != real || sources.library.is_some() != real {
        bail!("a real stage has the helper and the library, a rehearsal's has neither");
    }
    private_dir(&dir.bin_dir())?;
    let _ = std::fs::remove_file(dir.stamp_file());
    let app_sha256 = stage_file(sources.app, &dir.app_copy(), PROGRAM_MODE)?;
    let helper_sha256 = sources
        .helper
        .map(|helper| stage_file(helper, &dir.helper_copy(), PROGRAM_MODE))
        .transpose()?;
    let library = match sources.library {
        Some(lib) => {
            let name = lib
                .path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .filter(|n| is_library_name(n))
                .ok_or_else(|| anyhow!("the library's file name is not libtdjson.so.<version>"))?;
            private_dir(&dir.tdlib_dir())?;
            let staged = dir.tdlib_dir().join(&name);
            let sum = stage_file(&lib.path, &staged, LIBRARY_MODE)?;
            // The build took minutes: the file that was copied now must be the file that was checked.
            // Else the copy goes and no stamp is written.
            if sum != lib.sha256 {
                let _ = std::fs::remove_file(&staged);
                bail!("{LIBRARY_NOT_CHECKED}. Nothing was stamped.");
            }
            Some((name, sum))
        }
        None => None,
    };
    let stamp = Stamp {
        commit: commit.map(str::to_owned),
        app_sha256,
        helper_sha256,
        library,
    };
    write_private(&dir.stamp_file(), &format!("{:#}\n", stamp.to_json(now_ms)))?;
    Ok(stamp)
}

fn short(rev: &str) -> &str {
    &rev[..rev.len().min(10)]
}

/// One staged file against its stamp: a regular file (not a link), the mode, and the sum.
fn check_staged(path: &Path, mode: u32, sum: &str, what: &str) -> Result<()> {
    let meta = std::fs::symlink_metadata(path)
        .with_context(|| format!("the staged {what} is missing: start without --no-build"))?;
    if !meta.file_type().is_file() {
        bail!(
            "the staged {what} is not a regular file: start without --no-build. Nothing was started."
        );
    }
    let found = meta.mode() & 0o7777;
    if found != mode {
        bail!(
            "the staged {what} has mode {found:o}, not {mode:o}: start without --no-build to make it again. Nothing was started."
        );
    }
    let now = if mode == PROGRAM_MODE {
        sha256_of_program(path)?
    } else {
        sha256_of(path)?
    };
    if now != sum {
        bail!(
            "the staged {what} is not the file that was stamped (its sum differs): start without --no-build. Nothing was started."
        );
    }
    Ok(())
}

/// `--no-build`: the profile's copies are reused only when the stamp can be read, its commit is
/// `expect` (main's HEAD; a rehearsal has none and so must its stamp) and every file is what the stamp
/// says (B-1).
pub(super) fn check_stamped(dir: &ProfileDir, expect: Option<&str>) -> Result<Stamp> {
    let stamp = read_json(&dir.stamp_file())
        .ok()
        .flatten()
        .and_then(|v| Stamp::from_json(&v))
        .ok_or_else(|| {
            anyhow!(
                "this profile has no stamped copies, or their stamp is unreadable: start without --no-build. Nothing was started."
            )
        })?;
    if stamp.commit.as_deref() != expect {
        bail!(
            "the stamped copies were built at {}, and this start runs at {}: start without --no-build. Nothing was started.",
            stamp.commit.as_deref().map_or("no commit", short),
            expect.map_or("no commit", short)
        );
    }
    check_staged(&dir.app_copy(), PROGRAM_MODE, &stamp.app_sha256, "app")?;
    if let Some(sum) = &stamp.helper_sha256 {
        check_staged(&dir.helper_copy(), PROGRAM_MODE, sum, "helper")?;
    }
    if let Some((name, sum)) = &stamp.library {
        check_staged(&dir.tdlib_dir().join(name), LIBRARY_MODE, sum, "library")?;
    }
    Ok(stamp)
}

// ---- The build and the stage ------------------------------------------------------------------------

/// The arguments of the tool's own build, exactly (B-1): the app and, for a real run, the helper; no
/// `--tests`, no `--all-targets`, no `--workspace`, no feature flag.
pub(super) fn build_args(rehearsal: bool) -> &'static [&'static str] {
    if rehearsal {
        &["build", "-p", "kourier"]
    } else {
        &["build", "-p", "kourier", "-p", "kourier-tdlib"]
    }
}

pub(super) fn build_command(rehearsal: bool) -> Command {
    let mut cmd = crate::cargo();
    cmd.args(build_args(rehearsal));
    cmd
}

/// The real build (`crate::cargo()` scrubs the API variables).
pub(super) fn real_build(rehearsal: bool) -> Result<()> {
    crate::run("build kourier (debug)", &mut build_command(rehearsal))
}

/// What a stage uses of the machine.
pub(super) struct StageSeams<'a> {
    /// Builds (`true`: the rehearsal's build).
    pub build: &'a dyn Fn(bool) -> Result<()>,
    /// The build outputs.
    pub app: PathBuf,
    pub helper: PathBuf,
    /// The checked library (a real run).
    pub library: Option<CheckedLibrary>,
    /// main's HEAD now: the build took time and the tree must still be what the preflight saw.
    pub head_now: &'a dyn Fn() -> Result<String>,
    pub now_ms: u64,
}

/// Stages the copies: reuses the profile's own (`no_build`) or builds and copies. `head` is main's HEAD
/// of a real run (the stamp's commit) and `None` for the rehearsal.
pub(super) fn stage(
    dir: &ProfileDir,
    head: Option<&str>,
    no_build: bool,
    seams: &StageSeams<'_>,
) -> Result<Stamp> {
    if no_build {
        let stamp = check_stamped(dir, head)?;
        // A real run reuses the staged library only when it is the one the preflight just checked.
        if head.is_some() {
            let staged = stamp.library.as_ref().map(|(_, sum)| sum.as_str());
            if staged != seams.library.as_ref().map(|l| l.sha256.as_str()) {
                bail!("{LIBRARY_NOT_CHECKED}. Nothing was started.");
            }
        }
        return Ok(stamp);
    }
    (seams.build)(head.is_none())?;
    if let Some(head) = head
        && (seams.head_now)()? != head
    {
        bail!("main moved while the app was built: nothing was stamped. Start again.");
    }
    let sources = Sources {
        app: &seams.app,
        helper: head.map(|_| seams.helper.as_path()),
        library: if head.is_some() {
            seams.library.as_ref()
        } else {
            None
        },
    };
    stamp_copies(dir, &sources, head, seams.now_ms)?;
    check_stamped(dir, head)
}

// ---- The switch file --------------------------------------------------------------------------------

/// Whether `line` is a `networks-on` or `pending-link` line of a `kourier.toml`.
fn switch_key(line: &str) -> Option<&str> {
    let line = line.trim();
    ["networks-on", "pending-link"].into_iter().find(|key| {
        line.strip_prefix(key)
            .map(str::trim_start)
            .is_some_and(|r| r.starts_with('='))
    })
}

/// The profile's `kourier.toml` with `networks-on = ["Telegram"]` and `pending-link = "Telegram"` at
/// the top (a key after a `[table]` would belong to the table) and every other line of `existing` kept
/// as it was. An old switch is dropped, an array of `networks-on` that runs over several lines with it.
pub(super) fn switch_text(existing: Option<&str>) -> String {
    let mut out = String::from("networks-on = [\"Telegram\"]\npending-link = \"Telegram\"\n");
    let mut in_array = false;
    for line in existing.unwrap_or_default().lines() {
        if in_array {
            in_array = !line.contains(']');
            continue;
        }
        if let Some(key) = switch_key(line) {
            in_array = key == "networks-on" && line.contains('[') && !line.contains(']');
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Writes the switch file before a start (D-8): the one-shot `pending-link` is read AND CLEARED by the
/// app's start, so it is written again before EVERY start. The user's go for the run is the
/// acknowledgement of the warning page the descriptor's risk notice would show; a run makes no account.
pub(super) fn write_switch(dir: &ProfileDir) -> Result<()> {
    let existing = match std::fs::read_to_string(dir.config_file()) {
        Ok(text) => Some(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).context("read the profile's kourier.toml"),
    };
    private_dir(&dir.app_home())?;
    private_dir(
        dir.config_file()
            .parent()
            .ok_or_else(|| anyhow!("the config directory"))?,
    )?;
    write_private(&dir.config_file(), &switch_text(existing.as_deref()))
}

/// The two switches as the file says them now: for a test and for the walk's last look.
pub(super) fn switches_set(dir: &ProfileDir) -> bool {
    std::fs::read_to_string(dir.config_file()).is_ok_and(|text| {
        let (names, pending) = read_switches(&text);
        names.as_deref() == Some(&["Telegram".to_owned()][..])
            && pending.as_deref() == Some("Telegram")
    })
}

// ---- The probe --------------------------------------------------------------------------------------

/// Every inherited `KOURIER_*` variable name: the app must see none of them (smoke.rs:838
/// `inherited_to_strip`). A leftover `KOURIER_DEV_REHEARSAL` would put the fake in a real run's place,
/// a `KOURIER_FAILPOINT` aborts the process, a leftover dial variable would hand a confirmation to a
/// start that never earned it.
pub(super) fn inherited_to_strip(keys: &[OsString]) -> Vec<&OsString> {
    keys.iter()
        .filter(|key| key.to_string_lossy().starts_with("KOURIER_"))
        .collect()
}

/// The environment of every start the tool makes that is not the walk's own: the profile, the data
/// home the app derives the run profile's keeper root from, silent, offscreen.
pub(super) fn base_env(cmd: &mut Command, home: &Path, data_home: &Path, inherited: &[OsString]) {
    for key in inherited_to_strip(inherited) {
        cmd.env_remove(key);
    }
    cmd.env("KOURIER_HOME", home)
        .env("XDG_DATA_HOME", data_home)
        .env(TELEGRAM_ENV, SERVERS_WORD)
        .env("QT_QPA_PLATFORM", "offscreen")
        .env("QT_QUICK_BACKEND", "software")
        .env("QT_SCALE_FACTOR", "1")
        .env("LANG", "en_US.UTF-8")
        .env("LANGUAGE", "en")
        // Silent: a program that makes a sound finds no audio server.
        .env("PULSE_SERVER", NO_AUDIO_SERVER)
        .env("PIPEWIRE_REMOTE", NO_PIPEWIRE)
        .env_remove("PULSE_RUNTIME_PATH");
}

/// The command of the probe: the staged app asked which networks it would register
/// (`--print-networks`, which opens nothing and contacts nothing) over this profile and the test
/// servers. It carries NO dial variable and no `KOURIER_*` but the profile, the servers and, in a
/// rehearsal, the switch that puts the fake in Telegram's place (critic 9; B-2): it cannot take a
/// confirmation.
pub(super) fn probe_command(
    bin: &Path,
    home: &Path,
    data_home: &Path,
    rehearsal: bool,
    inherited: &[OsString],
) -> Command {
    let mut cmd = Command::new(bin);
    cmd.arg("--print-networks");
    base_env(&mut cmd, home, data_home, inherited);
    if rehearsal {
        cmd.env(REHEARSAL_ENV, REHEARSAL_WORD);
    }
    cmd
}

/// The probe's answer: `networks registered=<kinds> off=<kinds> on=<kinds>` names Telegram in
/// `registered=`. Else the refusal says what the line said (kinds only; the line has nothing else).
pub(super) fn judge_probe(stdout: &str) -> Result<()> {
    let line = stdout
        .lines()
        .find(|l| l.starts_with("networks "))
        .ok_or_else(|| {
            anyhow!(
                "the staged app printed no `networks` line: it would not start from this profile. Nothing was started."
            )
        })?;
    let registered = line
        .split_whitespace()
        .find_map(|w| w.strip_prefix("registered="))
        .unwrap_or_default();
    if !registered.split(',').any(|k| k == "Telegram") {
        bail!(
            "the staged app would not register Telegram (it printed `{}`): its API file, helper or library were not found beside the staged copy, or the switch was not read. Nothing was started.",
            redact(line)
        );
    }
    Ok(())
}

/// What a bounded command did.
pub(super) struct Bounded {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr_lines: usize,
    pub stderr: String,
}

/// Runs `cmd` to its end for at most `wait.bound` on `wait.clock` (the process is killed at the
/// bound), looked at every `wait.poll`, its output read by threads of their own so a full pipe cannot
/// stop it.
pub(super) fn run_bounded(mut cmd: Command, wait: &Wait<'_>) -> Result<Bounded> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().context("start the staged app")?;
    let read = |mut pipe: Box<dyn std::io::Read + Send>| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = pipe.read_to_end(&mut bytes);
            String::from_utf8_lossy(&bytes).into_owned()
        })
    };
    let out = read(Box::new(child.stdout.take().context("stdout")?));
    let err = read(Box::new(child.stderr.take().context("stderr")?));
    let start = wait.clock.now_ms();
    let bound_ms = u64::try_from(wait.bound.as_millis()).unwrap_or(u64::MAX);
    let code = loop {
        if let Some(status) = child.try_wait()? {
            break status.code();
        }
        if wait.clock.now_ms().saturating_sub(start) >= bound_ms {
            let _ = child.kill();
            let _ = child.wait();
            bail!("the staged app did not finish its probe in time and was killed");
        }
        std::thread::sleep(wait.poll);
        wait.clock.idle();
    };
    let stdout = out.join().unwrap_or_default();
    let stderr = err.join().unwrap_or_default();
    Ok(Bounded {
        code,
        stdout,
        stderr_lines: stderr.lines().count(),
        stderr,
    })
}

/// Runs the probe and judges it. A failed probe says its exit status and how many lines it wrote to
/// stderr (they go to the profile's `app.log`, redacted, for a look), never the lines.
pub(super) fn probe(
    dir: &ProfileDir,
    bin: &Path,
    home: &Path,
    data_home: &Path,
    rehearsal: bool,
    inherited: &[OsString],
    wait: &Wait<'_>,
) -> Result<()> {
    let result = run_bounded(
        probe_command(bin, home, data_home, rehearsal, inherited),
        wait,
    )?;
    if result.stderr_lines > 0 {
        append_redacted(&dir.app_log(), "==== tg-run: probe", &result.stderr)?;
    }
    if result.code != Some(0) {
        bail!(
            "the staged app's probe failed (exit status {}, {} line(s) on stderr, kept redacted in the profile's app.log). Nothing was started.",
            result.code.map_or("none".to_owned(), |c| c.to_string()),
            result.stderr_lines
        );
    }
    judge_probe(&result.stdout)
}

/// Appends `mark` and the redacted lines of `text` to the app log (0600).
pub(super) fn append_redacted(path: &Path, mark: &str, text: &str) -> Result<()> {
    use std::io::Write as _;
    let mut log = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
        .context("open the app log")?;
    writeln!(log, "{mark}")?;
    for line in text.lines() {
        writeln!(log, "{}", redact(line))?;
    }
    Ok(())
}
