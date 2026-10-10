//! `cargo xtask smoke <command>`: the smoke run of the real app against a Signal server, step by
//! step, on a profile that keeps its login (`docs/smoke-run.md` is the runbook).
//!
//! The debug build of Kourier runs on a profile directory of its own and is driven through its
//! development control channel (`--dev-control`, `crates/kourier-qt/src/dev`). A small
//! supervisor process stays with the app between commands, so every command here is a
//! separate, short shell call:
//!
//! ```text
//! cargo xtask smoke start --env <production|staging|local:<handshake.json>> [--profile <name>]
//! cargo xtask smoke step <name>      # one named step: what to do on the phone, the proof, PASS or FAIL
//! cargo xtask smoke state            # the profile and the running app, as counts and states
//! cargo xtask smoke report           # a Markdown summary into the profile directory
//! cargo xtask smoke stop             # stops the app; the login stays
//! cargo xtask smoke rehearse         # the whole tool against the mock server (no real server)
//! ```
//!
//! # The profile is a keeper
//!
//! A profile linked for a real server is linked once and used by every later run. It lives in
//! `${XDG_DATA_HOME:-~/.local/share}/kourier-smoke/<name>` (mode 0700), outside the repository
//! and its build directories and apart from the app's own data directory. The directory is
//! the whole login: the app gets it as `KOURIER_HOME` with the development keyring
//! (`KOURIER_DEV_KEYRING=passphrase`), so the desktop's keyring is never touched and no other
//! profile's key can be read, replaced or deleted. That keyring's passphrase is a public
//! constant: the database encryption protects nothing here. At rest the data is protected by
//! the disk encryption below the directory (checked at `start`) and by its mode.
//!
//! Nothing in the default run ends the login. `stop` only stops the app. The steps that end it
//! (`unlink-from-phone`) and the commands that delete it (`wipe`) need a flag that says so.
//! One app per profile: the supervisor holds an exclusive lock for the app's whole life, and
//! the app holds one of its own on its data directory. A profile belongs to the server it was
//! made for; a start that names another one is refused here, and by the app itself.
//!
//! # What the lead may read
//!
//! Everything this tool prints or writes below `<profile>/smoke/` is meant to be read by
//! someone who must not see names, numbers or message texts: steps find chats by kind and
//! count (dev-control `chats`, never by title), the QR link never leaves the app in a run
//! with a real server (`KOURIER_DEV_QR_URL=0`), the app's log is filtered at a fixed level and
//! passes a redaction of whole UUIDs, phone numbers and `sgnl://` links on its way to
//! `smoke/app.log`, and commands are recorded by name only, never with their arguments.
//! Screenshots are never taken.
//!
//! A local environment (`local:<handshake.json>`, the mock harness) gets a throwaway directory
//! below the target directory instead and runs offscreen, so mock runs never touch a keeper.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::{BufRead as _, BufReader, Read as _, Seek as _, SeekFrom, Write as _};
use std::os::unix::fs::{
    DirBuilderExt as _, MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _,
};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

use super::{cargo, root, run, target_dir};

const USAGE: &str = "usage: cargo xtask smoke <command> [options]

commands:
  start      start the app on the profile and keep it running
             --env <production|staging|local:<handshake.json>>   (required)
             --confirmed-by-user     required for production and staging: the user said go
             --expect-tail <digits>  last digits of the test account's number (first link)
             --max-sends <n>         cap of messages the tool may send on this profile run (30)
             --offscreen | --window  no window (default for local) or a window (default else)
             --desktop               notifications, tray and bus name on (off by default)
             --allow-schema-change   start a build whose database schema is newer than the profile's
             --history-transfer      offer the phone the history transfer at a first link
                                     (experimental: the import is not whole yet)
             --allow-unencrypted-disk  keep a real login on a disk without encryption
             --no-build              do not rebuild the app first
  step <name> [--wait <s>] [--seconds <s>] [--timer <off|s>] [--expect <n>] [--device-name <text>]
             one named step (see `steps`); --timer is what the PERSONAL phone shows for the
             1:1 chat: required by `send`, and by any other 1:1 sending step run before it
  steps      list the steps in their order
  state      the profile and the running app
  report     write smoke/report.md into the profile directory
  stop       stop the app; the login stays   [--force]
  backup     copy the whole profile directory (app stopped)   [--to <dir>]
  wipe       delete the profile directory for good   --yes-delete-the-login
  rehearse   run the whole tool against the mock server (throwaway profile, offscreen)   [--keep]

profile (every command): --profile <name> (default \"default\") | --home <dir> | --mock";

/// The default profile name.
const DEFAULT_PROFILE: &str = "default";
/// The directory below the data home that holds the kept profiles.
const KEEPER_DIR: &str = "kourier-smoke";
/// The tool's own files inside a profile directory.
const SMOKE_DIR: &str = "smoke";
/// Default cap on messages the tool sends on one profile between two `start --max-sends`.
const DEFAULT_MAX_SENDS: u64 = 30;

/// Signal removes a linked device that has not connected for a while. The period is in
/// neither Signal Desktop's sources nor the server's public documentation available offline;
/// Signal's support text says 30 days, which is used as the deadline here.
const IDLE_DEADLINE_DAYS: u64 = 30;
/// The tool warns from this many days without a connection.
const IDLE_WARN_DAYS: u64 = 21;

/// The app's log filter for a smoke run: every request with its route, status and timing
/// (`kourier_signal::requests`), Kourier's own crates at DEBUG, libsignal's protocol crate
/// capped at WARN (at INFO it prints whole service ids), nothing at a bare `debug`.
const LOG_FILTER: &str = "info,kourier_signal=debug,kourier_signal::requests=debug,\
kourier_core=debug,kourier_app=debug,libsignal_protocol=warn";

/// The name the device gets on the phone's list unless `--device-name` says otherwise.
const DEVICE_NAME: &str = "Kourier smoke test";

const WHAT_LINKING_DOES: &str = "What linking does to the account: it adds one new linked \
device to the test account (the phone lists it and shows Signal's notice about a new linked \
device); it uploads that device's keys (a signed prekey and a Kyber last-resort key for the \
ACI and the PNI identity, then 100 one-time prekeys of each kind per identity); it sets the \
device's capabilities to Signal Desktop's values; it asks the phone for its contacts, settings \
and block list and reads the account's Storage Service records and groups; from then on the \
device receives the account's messages, sends delivery receipts for them, and marks its \
last-seen time at every connect. Kourier has no request that writes the profile, the \
username, the PIN, a group or the Storage Service. Contacts' apps learn of the new device at \
their next send, without a safety-number change. To undo: on the test phone, Settings > \
Linked devices > the Kourier entry > Unlink.";

// ---- Small helpers ----------------------------------------------------------------------------

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// `YYYY-MM-DD HH:MM:SS UTC` for a time in ms since the epoch.
fn utc(ms: u64) -> String {
    let (y, m, d, hh, mm, ss) = civil(ms / 1000);
    format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02}:{ss:02} UTC")
}

/// `YYYYMMDD-HHMMSS` for directory names.
fn utc_compact(ms: u64) -> String {
    let (y, m, d, hh, mm, ss) = civil(ms / 1000);
    format!("{y:04}{m:02}{d:02}-{hh:02}{mm:02}{ss:02}")
}

/// Civil date and time of `secs` since the epoch (UTC; Howard Hinnant's algorithm).
fn civil(secs: u64) -> (u64, u64, u64, u64, u64, u64) {
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + u64::from(m <= 2);
    (y, m, d, rem / 3600, rem % 3600 / 60, rem % 60)
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Takes out of a line what must not reach a log a third party reads: a whole UUID keeps its
/// last four digits (`…1f2e`), a phone number becomes `+[number]`, a `sgnl://` or
/// `signalcaptcha://` link keeps only its scheme. The app's own log writer does the same
/// (`kourier_app::logging::redact`); this is the second net, for what does not come through
/// that writer (Qt's own lines, panics).
fn redact(line: &str) -> String {
    let b = line.as_bytes();
    let mut out = String::with_capacity(line.len());
    let mut copied = 0;
    let mut i = 0;
    while i < b.len() {
        let word_before = i > 0 && b[i - 1].is_ascii_alphanumeric();
        let hit: Option<(usize, String)> =
            if b[i].is_ascii_hexdigit() && (i == 0 || !b[i - 1].is_ascii_hexdigit()) {
                uuid_at(&b[i..]).then(|| (36, format!("…{}", &line[i + 32..i + 36])))
            } else if b[i] == b'+' && !word_before {
                let digits = b[i + 1..].iter().take_while(|c| c.is_ascii_digit()).count();
                (7..=15)
                    .contains(&digits)
                    .then(|| (1 + digits, "+[number]".to_owned()))
            } else if b[i] == b's' && !word_before {
                ["sgnl://", "signalcaptcha://"]
                    .into_iter()
                    .find(|scheme| b[i..].starts_with(scheme.as_bytes()))
                    .map(|scheme| {
                        let end = b[i..]
                            .iter()
                            .position(|c| {
                                c.is_ascii_whitespace()
                                    || matches!(c, b'"' | b'\'' | b')' | b']' | b'>')
                            })
                            .unwrap_or(b.len() - i);
                        (end, scheme)
                    })
                    // The bare scheme is a word of a message, not a link.
                    .filter(|(end, scheme)| *end > scheme.len())
                    .map(|(end, scheme)| (end, format!("{scheme}[redacted]")))
            } else {
                None
            };
        match hit {
            Some((n, replacement)) => {
                out.push_str(&line[copied..i]);
                out.push_str(&replacement);
                i += n;
                copied = i;
            }
            None => i += 1,
        }
    }
    out.push_str(&line[copied..]);
    out
}

/// Whether `b` starts with a UUID (`8-4-4-4-12` hex digits) that is not the head of a longer
/// run of hex digits.
fn uuid_at(b: &[u8]) -> bool {
    b.len() >= 36
        && b[..36].iter().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => *c == b'-',
            _ => c.is_ascii_hexdigit(),
        })
        && b.get(36).is_none_or(|c| !c.is_ascii_hexdigit())
}

/// Creates `dir` with mode 0700 (and missing parents) and tightens an existing one.
fn private_dir(dir: &Path) -> Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("create {}", dir.display()))?;
    let meta = std::fs::metadata(dir)?;
    if meta.permissions().mode() & 0o777 != 0o700 {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Writes `text` to `path` (mode 0600) through a temporary file and a rename.
fn write_private(path: &Path, text: &str) -> Result<()> {
    let tmp = path.with_extension("tmp");
    {
        let mut f = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .with_context(|| format!("write {}", tmp.display()))?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path).with_context(|| format!("replace {}", path.display()))?;
    Ok(())
}

fn append_line(path: &Path, line: &str) -> Result<()> {
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("append to {}", path.display()))?;
    writeln!(f, "{line}")?;
    Ok(())
}

fn read_json(path: &Path) -> Result<Option<Value>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(
            serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))?,
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

// ---- The server environment -------------------------------------------------------------------

/// `--env`.
#[derive(Clone, Debug, PartialEq, Eq)]
enum EnvArg {
    Production,
    Staging,
    /// A mock harness handshake file (absolute).
    Local(PathBuf),
}

impl EnvArg {
    fn parse(value: &str) -> Result<Self> {
        match value {
            "production" => Ok(Self::Production),
            "staging" => Ok(Self::Staging),
            _ => match value.strip_prefix("local:").filter(|f| !f.is_empty()) {
                Some(file) => Ok(Self::Local(
                    std::path::absolute(file).with_context(|| format!("--env {value}"))?,
                )),
                None => bail!("--env must be production, staging or local:<handshake.json>"),
            },
        }
    }

    fn is_local(&self) -> bool {
        matches!(self, Self::Local(_))
    }

    /// The value of `KOURIER_SIGNAL_ENV` for the app.
    fn for_app(&self) -> String {
        match self {
            Self::Production => "production".to_owned(),
            Self::Staging => "staging".to_owned(),
            Self::Local(file) => format!("local:{}", file.display()),
        }
    }

    /// The server a profile is bound to, in the form the app stores at link (`account.env`):
    /// `production`, `staging` or `local:<host>:<port>` of the handshake.
    fn binding(&self) -> Result<String> {
        match self {
            Self::Production => Ok("production".to_owned()),
            Self::Staging => Ok("staging".to_owned()),
            Self::Local(file) => {
                let hs = read_json(file)?
                    .ok_or_else(|| anyhow!("the handshake {} does not exist", file.display()))?;
                let (Some(host), Some(port)) = (hs["host"].as_str(), hs["port"].as_u64()) else {
                    bail!("{} is not a mock harness handshake", file.display());
                };
                Ok(format!("local:{host}:{port}"))
            }
        }
    }
}

// ---- The profile directory --------------------------------------------------------------------

/// Where a profile is, as the command line says it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Where {
    profile: Option<String>,
    home: Option<PathBuf>,
    mock: bool,
}

/// A resolved profile directory.
#[derive(Clone, Debug)]
struct ProfileDir {
    /// The directory (the app's `KOURIER_HOME`).
    dir: PathBuf,
    /// Whether it is a kept profile below the data home (not `--home`, not the mock's).
    keeper: bool,
}

fn valid_profile_name(name: &str) -> bool {
    (1..=32).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// The directory of the kept profiles: `${XDG_DATA_HOME:-~/.local/share}/kourier-smoke`.
fn keeper_root(env: &dyn Fn(&str) -> Option<String>) -> Result<PathBuf> {
    let abs = |k: &str| env(k).map(PathBuf::from).filter(|p| p.is_absolute());
    let data = abs("XDG_DATA_HOME")
        .or_else(|| abs("HOME").map(|h| h.join(".local/share")))
        .ok_or_else(|| anyhow!("neither XDG_DATA_HOME nor HOME is set"))?;
    Ok(data.join(KEEPER_DIR))
}

/// The throwaway directory of mock runs.
fn mock_dir() -> PathBuf {
    target_dir().join("smoke-mock").join(DEFAULT_PROFILE)
}

impl Where {
    /// The directory this names. A local environment means the mock's throwaway directory
    /// (or `--home`); it never takes `--profile`, so a mock run cannot make, use or stand in
    /// the way of a kept profile.
    fn resolve(
        &self,
        env: Option<&EnvArg>,
        vars: &dyn Fn(&str) -> Option<String>,
    ) -> Result<ProfileDir> {
        let named = usize::from(self.profile.is_some())
            + usize::from(self.home.is_some())
            + usize::from(self.mock);
        if named > 1 {
            bail!("give one of --profile, --home and --mock");
        }
        if let Some(home) = &self.home {
            let dir =
                std::path::absolute(home).with_context(|| format!("--home {}", home.display()))?;
            return Ok(ProfileDir { dir, keeper: false });
        }
        let local = env.is_some_and(EnvArg::is_local);
        if local && self.profile.is_some() {
            bail!(
                "--profile names a kept profile, and those are for real servers only. A local test server uses the throwaway profile (no option, or --mock) or --home <dir>"
            );
        }
        if self.mock || local {
            if env.is_some_and(|e| !e.is_local()) {
                bail!(
                    "--mock is the mock server's throwaway profile; a real server needs --profile or --home"
                );
            }
            return Ok(ProfileDir {
                dir: mock_dir(),
                keeper: false,
            });
        }
        let name = self.profile.as_deref().unwrap_or(DEFAULT_PROFILE);
        if !valid_profile_name(name) {
            bail!("invalid profile name (1-32 letters, digits, '-' or '_')");
        }
        Ok(ProfileDir {
            dir: keeper_root(vars)?.join(name),
            keeper: true,
        })
    }
}

impl ProfileDir {
    fn smoke(&self) -> PathBuf {
        self.dir.join(SMOKE_DIR)
    }
    fn state_file(&self) -> PathBuf {
        self.smoke().join("state.json")
    }
    fn launch_file(&self) -> PathBuf {
        self.smoke().join("launch.json")
    }
    fn socket(&self) -> PathBuf {
        self.smoke().join("control.sock")
    }
    fn lock_file(&self) -> PathBuf {
        self.smoke().join("lock")
    }
    fn supervisor_file(&self) -> PathBuf {
        self.smoke().join("supervisor.json")
    }
    fn app_log(&self) -> PathBuf {
        self.smoke().join("app.log")
    }
    fn events_log(&self) -> PathBuf {
        self.smoke().join("events.jsonl")
    }
    fn steps_log(&self) -> PathBuf {
        self.smoke().join("steps.jsonl")
    }
    fn supervisor_log(&self) -> PathBuf {
        self.smoke().join("supervisor.log")
    }
    fn report(&self) -> PathBuf {
        self.smoke().join("report.md")
    }
    fn baseline(&self) -> PathBuf {
        self.smoke().join("offline-baseline.json")
    }
    /// The app's own data directory inside the profile (`KOURIER_HOME` layout).
    fn app_data(&self) -> PathBuf {
        self.dir.join("kourier").join("data")
    }
    /// The app's `core.db` (its chats and messages).
    fn core_db(&self) -> PathBuf {
        self.app_data().join("core.db")
    }
}

/// Why `dir` must not hold a real login, if anything: inside the repository or a build
/// directory (where `git clean`, `cargo clean` or a tidy-up reaches it), or a place the app's
/// normal profile lives in.
fn unsafe_place_for_a_login(dir: &Path, vars: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    for (what, base) in [
        ("the repository", root()),
        ("the cargo target directory", target_dir()),
    ] {
        if dir.starts_with(&base) {
            return Some(format!(
                "{} is inside {what} ({}): a clean-up there would delete the login",
                dir.display(),
                base.display()
            ));
        }
    }
    let home = vars("HOME").map(PathBuf::from);
    let xdg = |var: &str, fallback: &str| {
        vars(var)
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| home.as_ref().map(|h| h.join(fallback)))
    };
    let bases = [
        home.clone(),
        xdg("XDG_DATA_HOME", ".local/share"),
        xdg("XDG_CONFIG_HOME", ".config"),
        xdg("XDG_STATE_HOME", ".local/state"),
        xdg("XDG_CACHE_HOME", ".cache"),
    ];
    if bases.iter().flatten().any(|b| b == dir) {
        return Some(format!(
            "{} is a base directory of the user's own files: Kourier's normal profile would share it",
            dir.display()
        ));
    }
    // A normal profile keeps `core.db` and `accounts/` directly in `<data home>/kourier`.
    let normal = dir.join("kourier");
    if normal.join("core.db").exists() || normal.join("accounts").exists() {
        return Some(format!(
            "{} already holds a normal Kourier profile",
            normal.display()
        ));
    }
    None
}

/// What protects the directory at rest: `Some(device)` when the file system below `dir` is
/// on a dm-crypt device (LUKS), looked up from its device number in sysfs.
fn disk_encryption(dir: &Path) -> Option<String> {
    let existing = dir.ancestors().find(|p| p.exists())?;
    let dev = std::fs::metadata(existing).ok()?.dev();
    let (major, minor) = (libc::major(dev), libc::minor(dev));
    crypt_below(&PathBuf::from(format!("/sys/dev/block/{major}:{minor}")), 0)
}

/// Walks a block device and the devices it sits on, looking for a dm-crypt one.
fn crypt_below(sys: &Path, depth: u32) -> Option<String> {
    if depth > 6 {
        return None;
    }
    if let Ok(uuid) = std::fs::read_to_string(sys.join("dm/uuid"))
        && uuid.starts_with("CRYPT-")
    {
        let name = std::fs::read_to_string(sys.join("dm/name")).unwrap_or_default();
        return Some(format!("dm-crypt device {}", redact(name.trim())));
    }
    // A partition has no `slaves` of its own: its disk is the parent directory.
    let slaves = std::fs::read_dir(sys.join("slaves")).ok()?;
    slaves
        .flatten()
        .find_map(|slave| crypt_below(&slave.path(), depth + 1))
}

// ---- The tool's state file --------------------------------------------------------------------

/// `<profile>/smoke/state.json`: what the tool knows about the profile without opening its
/// encrypted databases. Written by `start` before the app runs and by the supervisor while
/// it does.
#[derive(Clone, Debug, PartialEq)]
struct State(Value);

impl State {
    fn new(binding: &str, env_arg: &str, max_sends: u64, now: u64) -> Self {
        Self(json!({
            "version": 1,
            "env": binding,
            "env_arg": env_arg,
            "created_ms": now,
            "linked_since_ms": null,
            "last_online_ms": null,
            "logged_out_ms": null,
            "account_tail": null,
            "device_id": null,
            "build_commit": null,
            "schema": null,
            "sends_used": 0,
            "max_sends": max_sends,
            "peer_conv": null,
            "group_conv": null,
            "peer_timer_secs": null,
            "chats_db": null,
            "starts": 0,
        }))
    }

    fn load(dir: &ProfileDir) -> Result<Option<Self>> {
        Ok(read_json(&dir.state_file())?.map(Self))
    }

    fn save(&self, dir: &ProfileDir) -> Result<()> {
        write_private(&dir.state_file(), &format!("{:#}\n", self.0))
    }

    fn str(&self, key: &str) -> Option<&str> {
        self.0[key].as_str()
    }

    fn num(&self, key: &str) -> Option<u64> {
        self.0[key].as_u64()
    }

    fn set(&mut self, key: &str, value: Value) {
        self.0[key] = value;
    }

    fn linked(&self) -> bool {
        self.num("linked_since_ms").is_some() && self.num("logged_out_ms").is_none()
    }
}

/// What tells one file from another that took its place: its device and inode numbers.
/// `None` when the file is not there.
fn file_id(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    Some(format!("{}:{}", meta.dev(), meta.ino()))
}

/// The remembered chats (`peer_conv`, `group_conv`) are row numbers of the app's `core.db`.
/// A `core.db` that was rebuilt (lost, reset) or restored from a copy numbers its chats anew,
/// and a remembered number could then mean another chat: a send would go to the wrong
/// people. So the chats are remembered together with the database file they were seen in
/// (`chats_db`), and forgotten when that is not the file that is there now. Returns whether
/// anything was forgotten.
fn forget_chats_of_another_db(state: &mut State, core_db: &Path) -> bool {
    let remembers = !state.0["peer_conv"].is_null() || !state.0["group_conv"].is_null();
    let same_db = state
        .str("chats_db")
        .is_some_and(|seen_in| file_id(core_db).as_deref() == Some(seen_in));
    if !remembers || same_db {
        return false;
    }
    for key in ["peer_conv", "group_conv", "peer_timer_secs", "chats_db"] {
        state.set(key, Value::Null);
    }
    true
}

/// What to say about the time since the device last connected: nothing, a warning, or that
/// the deadline has passed.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Idle {
    /// Connected recently enough (days ago).
    Fine(u64),
    /// Close to the deadline (days ago).
    Warn(u64),
    /// Past the deadline (days ago).
    Overdue(u64),
}

fn idle(last_online_ms: u64, now: u64) -> Idle {
    let days = now.saturating_sub(last_online_ms) / 86_400_000;
    if days >= IDLE_DEADLINE_DAYS {
        Idle::Overdue(days)
    } else if days >= IDLE_WARN_DAYS {
        Idle::Warn(days)
    } else {
        Idle::Fine(days)
    }
}

/// The lines `start` and `state` print about a linked profile's age and last connection.
fn login_lines(state: &State, now: u64) -> Vec<String> {
    let mut lines = Vec::new();
    let Some(since) = state.num("linked_since_ms") else {
        lines.push("not linked yet".to_owned());
        return lines;
    };
    if let Some(out) = state.num("logged_out_ms") {
        lines.push(format!(
            "the login ended on {} (the app reported logged out); it was linked since {}",
            utc(out),
            utc(since)
        ));
        return lines;
    }
    lines.push(format!("linked since {}", utc(since)));
    match state.num("last_online_ms") {
        None => lines.push("never seen online by this tool".to_owned()),
        Some(last) => {
            let (days, note) = match idle(last, now) {
                Idle::Fine(d) => (d, String::new()),
                Idle::Warn(d) => (
                    d,
                    format!(
                        "\nWARNING: Signal removes a linked device that has not connected for about {IDLE_DEADLINE_DAYS} days. {} days are left: connect now.",
                        IDLE_DEADLINE_DAYS.saturating_sub(d)
                    ),
                ),
                Idle::Overdue(d) => (
                    d,
                    format!(
                        "\nWARNING: more than {IDLE_DEADLINE_DAYS} days without a connection. The server has probably removed this device; the app will then report \"logged out\". Do not link again before reading the log."
                    ),
                ),
            };
            lines.push(format!(
                "last connected {} ({days} days ago){note}",
                utc(last)
            ));
        }
    }
    lines
}

// ---- The supervisor ---------------------------------------------------------------------------

/// Events kept for clients that ask later.
const EVENT_RING: usize = 2000;
/// At a start, an `app.log` larger than this becomes `app.log.1` (replacing an older one).
const APP_LOG_KEEP_BYTES: u64 = 64 * 1024 * 1024;

struct Shared {
    dir: ProfileDir,
    local: bool,
    stdin: Option<ChildStdin>,
    pending: HashMap<u64, mpsc::Sender<Value>>,
    next_id: u64,
    events: VecDeque<(u64, Value)>,
    next_seq: u64,
    /// The newest QR link, kept only for a local environment (the mock phone "scans" it).
    qr_url: Option<String>,
    state: State,
    dirty: bool,
    /// Between the phone's answer and the end of the login: no stop without `--force`.
    link_in_flight: bool,
    /// A login step was seen since the app started (a link is or was underway in this run).
    login_seen: bool,
    /// The dev channel is up.
    ready: bool,
    online: bool,
    app_pid: u32,
    started_ms: u64,
}

impl Shared {
    fn push_event(&mut self, event: Value) {
        let line = json!({"t": now_ms(), "seq": self.next_seq, "event": event});
        let _ = append_line(&self.dir.events_log(), &line.to_string());
        if self.events.len() >= EVENT_RING {
            self.events.pop_front();
        }
        self.events.push_back((self.next_seq, event));
        self.next_seq += 1;
    }

    /// Applies one protocol event of the app: sanitizes it, updates what the tool knows.
    fn on_app_event(&mut self, mut v: Value) {
        let name = v["event"].as_str().unwrap_or_default().to_owned();
        let now = now_ms();
        match name.as_str() {
            "channel" => self.ready = true,
            "login_qr" => {
                // The link is what the phone scans: never stored, never passed on. Only the
                // mock's phone gets it, from memory.
                let url = v["data"]
                    .as_object_mut()
                    .and_then(|d| d.remove("url"))
                    .and_then(|u| u.as_str().map(str::to_owned));
                if self.local {
                    self.qr_url = url;
                }
                v["data"]["shown"] = json!(true);
                self.link_in_flight = false;
            }
            "login_step" => {
                self.login_seen = true;
                match v["data"]["kind"].as_str() {
                    Some("progress") => self.link_in_flight = true,
                    Some("done") => {
                        self.link_in_flight = false;
                        self.qr_url = None;
                        self.state.set("linked_since_ms", json!(now));
                        self.state.set("logged_out_ms", Value::Null);
                        self.dirty = true;
                    }
                    Some(_) => self.link_in_flight = false,
                    None => {}
                }
            }
            "account_state" => {
                let conn = v["data"]["conn"].as_str().unwrap_or_default();
                let account = v["data"]["state"].as_str().unwrap_or_default();
                self.online = account == "active" && conn == "online";
                if let Some(device) = v["data"]["device"].as_u64()
                    && self.state.num("device_id") != Some(device)
                {
                    self.state.set("device_id", json!(device));
                    self.dirty = true;
                }
                if self.online {
                    self.state.set("last_online_ms", json!(now));
                    // An account that is online without a login in this run was linked
                    // before: a state file that lost that (the supervisor was killed between
                    // the end of a login and its next save) learns it again here, so the
                    // profile is not taken for one that still needs its first link.
                    if !self.login_seen && !self.state.linked() {
                        self.state.set("linked_since_ms", json!(now));
                        self.state.set("logged_out_ms", Value::Null);
                    }
                    self.dirty = true;
                }
                if account == "logged_out" && self.state.num("linked_since_ms").is_some() {
                    if self.state.num("logged_out_ms").is_none() {
                        self.state.set("logged_out_ms", json!(now));
                        self.dirty = true;
                    }
                    self.online = false;
                }
            }
            "error" => {
                if let Some(message) = v["data"]["message"].as_str() {
                    v["data"]["message"] = json!(redact(message));
                }
            }
            _ => {}
        }
        self.push_event(v);
    }

    fn save_state(&mut self) {
        if self.online {
            self.state.set("last_online_ms", json!(now_ms()));
        }
        if let Err(e) = self.state.save(&self.dir) {
            eprintln!("smoke supervisor: cannot write the state file: {e:#}");
        }
        self.dirty = false;
    }
}

/// The inherited variables the app must not see: every `KOURIER_*` one. Whatever the shell
/// has set for a debug build has no say in a smoke run: a leftover `KOURIER_SIGNAL_ENV` must
/// never point a kept login at another server, and `KOURIER_FAILPOINT` aborts the process.
fn inherited_to_strip(keys: impl Iterator<Item = std::ffi::OsString>) -> Vec<std::ffi::OsString> {
    keys.filter(|key| key.to_string_lossy().starts_with("KOURIER_"))
        .collect()
}

/// How long a launch order of `start` stays valid: the supervisor reads it within a moment.
const LAUNCH_VALID_MS: u64 = 120_000;

/// Reads the launch order `start` left for the supervisor and takes it away: one order
/// starts the app once. `start` is the only writer, and it writes after its checks (the
/// user's go for a real server, the profile's own server, the place, the disk). So the
/// supervisor command run by hand finds no order, or one that is too old, and starts
/// nothing: a real server is never reached around `--confirmed-by-user`.
fn take_launch(dir: &ProfileDir, state: &State, now: u64) -> Result<Value> {
    let file = dir.launch_file();
    let launch = read_json(&file)?;
    // Taken away before it is judged: a refused order must not wait for a second try.
    let _ = std::fs::remove_file(&file);
    let launch = launch.context(
        "no launch order: the supervisor is started by `cargo xtask smoke start`, never by hand",
    )?;
    let issued = launch["issued_ms"].as_u64().unwrap_or(0);
    if now.saturating_sub(issued) > LAUNCH_VALID_MS || issued > now + LAUNCH_VALID_MS {
        bail!("the launch order is stale: run `cargo xtask smoke start` again");
    }
    let env_arg = launch["env_arg"]
        .as_str()
        .context("the launch order names no server")?;
    if state.str("env_arg") != Some(env_arg) {
        bail!("the launch order names another server than the profile's state file");
    }
    Ok(launch)
}

/// The app's command line and environment for a smoke run. The server is always named: a
/// debug build without `KOURIER_SIGNAL_ENV` would talk to production.
fn app_command(bin: &Path, dir: &ProfileDir, launch: &Value) -> Result<Command> {
    let env_arg = launch["env_arg"]
        .as_str()
        .filter(|e| !e.is_empty())
        .context("the launch order names no server")?;
    let mut cmd = Command::new(bin);
    cmd.arg("--dev-control");
    for key in inherited_to_strip(std::env::vars_os().map(|(key, _)| key)) {
        cmd.env_remove(&key);
    }
    cmd.env("KOURIER_HOME", &dir.dir)
        .env("KOURIER_SIGNAL_ENV", env_arg)
        .env("KOURIER_DEV_KEYRING", "passphrase")
        // A crash must not leave the account's keys in a core dump.
        .env("KOURIER_DEV_NO_DUMP", "1")
        .env("KOURIER_LOG", LOG_FILTER)
        .env("QT_FORCE_STDERR_LOGGING", "1");
    if launch["local"] != json!(true) {
        cmd.env("KOURIER_DEV_QR_URL", "0");
    }
    if let Some(tail) = launch["expect_tail"].as_str() {
        cmd.env("KOURIER_DEV_EXPECT_NUMBER_TAIL", tail);
    }
    if launch["history_transfer"] == json!(true) {
        // Only the launch order switches the transfer on: an inherited variable was
        // stripped above with every other `KOURIER_` one.
        cmd.env("KOURIER_HISTORY_TRANSFER", "1");
    }
    if launch["desktop"] != json!(true) {
        // No notifications (they would show names and texts on the desktop and stay in its
        // history), no tray, no bus name: the profile's locks keep it to one process.
        cmd.env("KOURIER_DESKTOP", "0");
    }
    if launch["offscreen"] == json!(true) {
        cmd.arg("--size=1200x800")
            .env("QT_QPA_PLATFORM", "offscreen")
            .env("QT_QUICK_BACKEND", "software")
            .env("QT_SCALE_FACTOR", "1")
            .env("LANG", "en_US.UTF-8")
            .env("LANGUAGE", "en");
    }
    Ok(cmd)
}

/// `cargo xtask smoke __supervise --home <dir>`: runs the app and serves the control socket
/// until the app is gone. Started detached by `start`.
fn supervise(dir: &ProfileDir) -> Result<()> {
    let lock_file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(dir.lock_file())?;
    match lock_file.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => {
            bail!("another smoke run holds this profile's lock")
        }
        Err(std::fs::TryLockError::Error(e)) => return Err(e).context("lock the profile"),
    }
    let state = State::load(dir)?.context("no state.json")?;
    let launch = take_launch(dir, &state, now_ms())?;
    let bin = PathBuf::from(launch["bin"].as_str().context("launch.json: bin")?);
    let mut child = app_command(&bin, dir, &launch)?
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("start {}", bin.display()))?;
    let started_ms = now_ms();
    // The log of earlier runs is kept once: a long run at this log level writes a lot.
    if file_len(&dir.app_log()) > APP_LOG_KEEP_BYTES {
        let _ = std::fs::rename(dir.app_log(), dir.smoke().join("app.log.1"));
    }
    let shared = Arc::new(Mutex::new(Shared {
        dir: dir.clone(),
        local: launch["local"] == json!(true),
        stdin: child.stdin.take(),
        pending: HashMap::new(),
        next_id: 0,
        events: VecDeque::new(),
        next_seq: 0,
        qr_url: None,
        state,
        dirty: false,
        link_in_flight: false,
        login_seen: false,
        ready: false,
        online: false,
        app_pid: child.id(),
        started_ms,
    }));
    write_private(
        &dir.supervisor_file(),
        &json!({"pid": std::process::id(), "app_pid": child.id(), "started_ms": started_ms})
            .to_string(),
    )?;
    append_line(
        &dir.app_log(),
        &format!(
            "==== smoke: app started {} (pid {}) ====",
            utc(started_ms),
            child.id()
        ),
    )?;

    // The app's log: every stderr line, redacted, into smoke/app.log.
    let stderr = child.stderr.take().context("child stderr")?;
    let log_path = dir.app_log();
    std::thread::spawn(move || {
        let mut log = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&log_path)
            .ok();
        // The pipe is read to its end whether or not the file can be written: an app whose
        // stderr nobody reads stops at its next log line once the pipe is full.
        // (Line by line as bytes: a line that is not UTF-8 must not end the reading either.)
        let mut reader = BufReader::new(stderr);
        let mut line = Vec::new();
        loop {
            line.clear();
            match reader.read_until(b'\n', &mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            if let Some(log) = log.as_mut() {
                let text = String::from_utf8_lossy(&line);
                let _ = writeln!(log, "{}", redact(text.trim_end_matches(['\n', '\r'])));
            }
        }
    });
    // The dev channel: responses to whoever asked, events into the ring.
    let stdout = child.stdout.take().context("child stdout")?;
    let reader = shared.clone();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            let Ok(v @ Value::Object(_)) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let mut s = lock(&reader);
            match v["type"].as_str() {
                Some("response") => {
                    if let Some(tx) = v["id"].as_u64().and_then(|id| s.pending.remove(&id)) {
                        let _ = tx.send(v);
                    }
                }
                Some("event") => s.on_app_event(v),
                _ => {}
            }
        }
    });

    let socket = dir.socket();
    let _ = std::fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket).with_context(|| {
        format!(
            "bind {} (the path may be too long for a socket)",
            socket.display()
        )
    })?;
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    let mut last_save = Instant::now();
    let status = loop {
        match listener.accept() {
            Ok((stream, _)) => {
                let shared = shared.clone();
                std::thread::spawn(move || serve(&shared, stream));
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => eprintln!("smoke supervisor: accept: {e}"),
        }
        if let Some(status) = child.try_wait()? {
            break status;
        }
        {
            let mut s = lock(&shared);
            // "Last connected" follows a connection that stays up, once a minute.
            if s.dirty || (s.online && last_save.elapsed() >= Duration::from_secs(60)) {
                s.save_state();
                last_save = Instant::now();
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let mut s = lock(&shared);
    s.push_event(
        json!({"type": "event", "event": "app_exited", "data": {"status": status.to_string()}}),
    );
    s.save_state();
    let _ = append_line(
        &dir.app_log(),
        &format!("==== smoke: app exited {} ({status}) ====", utc(now_ms())),
    );
    let _ = std::fs::remove_file(&socket);
    let _ = std::fs::remove_file(dir.supervisor_file());
    drop(lock_file);
    Ok(())
}

/// Commands that make the app send a message: they need room in the send budget.
fn sends_messages(cmd: &str) -> bool {
    matches!(cmd, "send" | "act")
}

/// Serves one request of one client connection.
fn serve(shared: &Arc<Mutex<Shared>>, stream: UnixStream) {
    let mut line = String::new();
    let Ok(read_half) = stream.try_clone() else {
        return;
    };
    if BufReader::new(read_half).read_line(&mut line).is_err() {
        return;
    }
    let request: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
    let answer = handle(shared, &request);
    let mut stream = stream;
    let _ = writeln!(stream, "{answer}");
}

fn refuse(code: &str, message: &str) -> Value {
    json!({"ok": false, "error": {"code": code, "message": message}})
}

fn handle(shared: &Arc<Mutex<Shared>>, request: &Value) -> Value {
    match request["op"].as_str() {
        Some("info") => {
            let s = lock(shared);
            json!({
                "ok": true,
                "ready": s.ready,
                "online": s.online,
                "app_pid": s.app_pid,
                "started_ms": s.started_ms,
                "link_in_flight": s.link_in_flight,
                "state": s.state.0,
                "next_seq": s.next_seq,
                // Whether this user can read the app's memory map: only of a dumpable process.
                "dumpable": std::fs::read(format!("/proc/{}/smaps_rollup", s.app_pid)).is_ok(),
            })
        }
        Some("events") => {
            let since = request["since"].as_u64().unwrap_or(0);
            let s = lock(shared);
            let events: Vec<Value> = s
                .events
                .iter()
                .filter(|(seq, _)| *seq >= since)
                .map(|(seq, e)| json!({"seq": seq, "event": e["event"], "data": e["data"]}))
                .collect();
            json!({"ok": true, "events": events, "next_seq": s.next_seq})
        }
        Some("qr") => {
            let s = lock(shared);
            if !s.local {
                return refuse(
                    "refused",
                    "the link is handed out only for a local test server",
                );
            }
            json!({"ok": true, "url": s.qr_url})
        }
        Some("set") => {
            let mut s = lock(shared);
            for key in ["peer_conv", "group_conv", "peer_timer_secs"] {
                if let Some(v) = request.get(key) {
                    s.state.set(key, v.clone());
                }
            }
            // The chats are numbers of this database file and of no other.
            let db = file_id(&s.dir.core_db());
            s.state.set("chats_db", json!(db));
            s.save_state();
            json!({"ok": true})
        }
        Some("stop") => {
            let mut s = lock(shared);
            if s.link_in_flight && request["force"] != json!(true) {
                return refuse(
                    "linking",
                    "a link is in flight (between the phone's confirmation and \"done\"): stopping now can leave a device on the phone whose keys are gone. Wait for the link step to end, or use --force",
                );
            }
            s.next_id += 1;
            let id = s.next_id;
            if let Some(stdin) = s.stdin.as_mut() {
                let _ = writeln!(stdin, "{}", json!({"id": id, "cmd": "quit"}));
                let _ = stdin.flush();
            }
            // Closing stdin quits the app too, should the request be lost.
            s.stdin = None;
            s.push_event(json!({"type": "event", "event": "stop_requested", "data": {}}));
            json!({"ok": true})
        }
        Some("ctl") => ctl(shared, request),
        _ => refuse("badRequest", "unknown op"),
    }
}

/// Forwards one dev-control command to the app and returns its response. Commands are
/// recorded by name only: their arguments can carry a text.
fn ctl(shared: &Arc<Mutex<Shared>>, request: &Value) -> Value {
    let Some(cmd) = request["cmd"].as_str() else {
        return refuse("badRequest", "ctl without cmd");
    };
    let args = request.get("args").cloned().unwrap_or_else(|| json!({}));
    let timeout = Duration::from_millis(request["timeout_ms"].as_u64().unwrap_or(120_000));
    let sends = request["sends"].as_u64().unwrap_or(0);
    let started = Instant::now();
    let (id, rx) = {
        let mut s = lock(shared);
        if sends_messages(cmd) {
            // The hard cap on what the tool sends: counted before the command leaves, so a
            // command that is lost still counts, and nothing here ever sends twice.
            let used = s.state.num("sends_used").unwrap_or(0);
            let max = s.state.num("max_sends").unwrap_or(DEFAULT_MAX_SENDS);
            if sends == 0 {
                return refuse(
                    "budget",
                    "a sending command must say how many messages it sends",
                );
            }
            if used + sends > max {
                return refuse(
                    "budget",
                    &format!(
                        "the send cap of this run is reached ({used} of {max} used, {sends} more asked); start again with a higher --max-sends if the user agrees"
                    ),
                );
            }
            s.state.set("sends_used", json!(used + sends));
            s.save_state();
        }
        s.next_id += 1;
        let id = s.next_id;
        let (tx, rx) = mpsc::channel();
        let line = json!({"id": id, "cmd": cmd, "args": args});
        let Some(stdin) = s.stdin.as_mut() else {
            return refuse("gone", "the app is stopping");
        };
        if writeln!(stdin, "{line}")
            .and_then(|()| stdin.flush())
            .is_err()
        {
            return refuse("gone", "the app does not take commands any more");
        }
        s.pending.insert(id, tx);
        (id, rx)
    };
    let response = match rx.recv_timeout(timeout) {
        Ok(v) => v,
        Err(RecvTimeoutError::Timeout) => {
            lock(shared).pending.remove(&id);
            refuse("timeout", "the app did not answer in time")
        }
        Err(RecvTimeoutError::Disconnected) => refuse("gone", "the app exited"),
    };
    let ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let ok = response["ok"] == json!(true);
    let mut record =
        json!({"type": "event", "event": "ctl", "data": {"cmd": cmd, "ok": ok, "ms": ms}});
    if !ok {
        record["data"]["code"] = response["error"]["code"].clone();
    }
    if sends > 0 {
        record["data"]["sends"] = json!(sends);
    }
    lock(shared).push_event(record);
    if ok {
        json!({"ok": true, "result": response["result"]})
    } else {
        let message = response["error"]["message"].as_str().unwrap_or("failed");
        refuse(
            response["error"]["code"].as_str().unwrap_or("failed"),
            &redact(message),
        )
    }
}

// ---- The client side of the socket ------------------------------------------------------------

/// A failed dev-control command or supervisor request.
#[derive(Debug)]
struct Refused {
    code: String,
    message: String,
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.message, self.code)
    }
}

impl std::error::Error for Refused {}

struct Client {
    socket: PathBuf,
}

impl Client {
    /// The client of the supervisor of `dir`, when one runs.
    fn connect(dir: &ProfileDir) -> Result<Self> {
        let client = Self {
            socket: dir.socket(),
        };
        client
            .call(&json!({"op": "info"}), Duration::from_secs(5))
            .with_context(|| {
                format!(
                    "no smoke run is up on {} (start one with `cargo xtask smoke start`)",
                    dir.dir.display()
                )
            })?;
        Ok(client)
    }

    fn call(&self, request: &Value, timeout: Duration) -> Result<Value> {
        let mut stream = UnixStream::connect(&self.socket)
            .with_context(|| format!("connect to {}", self.socket.display()))?;
        stream.set_read_timeout(Some(timeout + Duration::from_secs(5)))?;
        writeln!(stream, "{request}")?;
        let mut line = String::new();
        BufReader::new(stream)
            .read_line(&mut line)
            .context("the supervisor did not answer")?;
        let answer: Value = serde_json::from_str(&line).context("the supervisor's answer")?;
        if answer["ok"] == json!(true) {
            Ok(answer)
        } else {
            Err(Refused {
                code: answer["error"]["code"]
                    .as_str()
                    .unwrap_or("failed")
                    .to_owned(),
                message: answer["error"]["message"]
                    .as_str()
                    .unwrap_or("failed")
                    .to_owned(),
            }
            .into())
        }
    }

    fn info(&self) -> Result<Value> {
        self.call(&json!({"op": "info"}), Duration::from_secs(5))
    }

    /// One dev-control command that sends nothing.
    fn ctl(&self, cmd: &str, args: Value, timeout: Duration) -> Result<Value> {
        debug_assert!(!sends_messages(cmd));
        self.ctl_raw(cmd, args, 0, timeout)
    }

    /// One dev-control command that makes the app send `sends` messages. Called once per
    /// send: nothing in this tool repeats it.
    fn ctl_sending(&self, cmd: &str, args: Value, sends: u64, timeout: Duration) -> Result<Value> {
        self.ctl_raw(cmd, args, sends, timeout)
    }

    fn ctl_raw(&self, cmd: &str, args: Value, sends: u64, timeout: Duration) -> Result<Value> {
        let ms = u64::try_from(timeout.as_millis()).unwrap_or(120_000);
        let answer = self.call(
            &json!({"op": "ctl", "cmd": cmd, "args": args, "sends": sends, "timeout_ms": ms}),
            timeout,
        )?;
        Ok(answer["result"].clone())
    }

    /// dev-control `wait`.
    fn wait(&self, until: &str, timeout: Duration) -> Result<Value> {
        let ms = u64::try_from(timeout.as_millis()).unwrap_or(60_000);
        self.ctl(
            "wait",
            json!({"until": until, "timeout_ms": ms}),
            timeout + Duration::from_secs(10),
        )
    }

    fn state(&self) -> Result<Value> {
        self.ctl("state", json!({}), Duration::from_secs(30))
    }

    /// The events from `since` on, and the next sequence number.
    fn events(&self, since: u64) -> Result<(Vec<Value>, u64)> {
        let answer = self.call(
            &json!({"op": "events", "since": since}),
            Duration::from_secs(5),
        )?;
        Ok((
            answer["events"].as_array().cloned().unwrap_or_default(),
            answer["next_seq"].as_u64().unwrap_or(since),
        ))
    }
}

// ---- Arguments --------------------------------------------------------------------------------

/// The parsed command line.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Args {
    command: String,
    step: Option<String>,
    place: Where,
    env: Option<EnvArg>,
    confirmed: bool,
    expect_tail: Option<String>,
    max_sends: Option<u64>,
    offscreen: Option<bool>,
    desktop: bool,
    allow_schema_change: bool,
    history_transfer: bool,
    allow_unencrypted: bool,
    no_build: bool,
    force: bool,
    to: Option<PathBuf>,
    yes_delete: bool,
    destroys_login: bool,
    keep: bool,
    wait: Option<u64>,
    seconds: Option<u64>,
    timer: Option<String>,
    expect: Option<u64>,
    device_name: Option<String>,
}

fn parse_args(args: &[String]) -> Result<Args> {
    let mut a = Args::default();
    let mut it = args.iter();
    a.command = match it.next() {
        Some(c) => c.clone(),
        None => bail!("{USAGE}"),
    };
    if a.command == "step" {
        a.step = Some(
            it.next()
                .filter(|s| !s.starts_with("--"))
                .ok_or_else(|| anyhow!("step needs a name (see `cargo xtask smoke steps`)"))?
                .clone(),
        );
    }
    while let Some(arg) = it.next() {
        let mut value = |what: &str| {
            it.next()
                .cloned()
                .ok_or_else(|| anyhow!("{arg} needs {what}"))
        };
        let number = |text: String| {
            text.parse::<u64>()
                .map_err(|_| anyhow!("{arg} needs a number"))
        };
        match arg.as_str() {
            "--profile" => a.place.profile = Some(value("a name")?),
            "--home" => a.place.home = Some(PathBuf::from(value("a directory")?)),
            "--mock" => a.place.mock = true,
            "--env" => a.env = Some(EnvArg::parse(&value("an environment")?)?),
            "--confirmed-by-user" => a.confirmed = true,
            "--expect-tail" => a.expect_tail = Some(value("digits")?),
            "--max-sends" => a.max_sends = Some(number(value("a number")?)?),
            "--offscreen" => a.offscreen = Some(true),
            "--window" => a.offscreen = Some(false),
            "--desktop" => a.desktop = true,
            "--allow-schema-change" => a.allow_schema_change = true,
            "--history-transfer" => a.history_transfer = true,
            "--allow-unencrypted-disk" => a.allow_unencrypted = true,
            "--no-build" => a.no_build = true,
            "--force" => a.force = true,
            "--to" => a.to = Some(PathBuf::from(value("a directory")?)),
            "--yes-delete-the-login" => a.yes_delete = true,
            "--destroys-login" => a.destroys_login = true,
            "--keep" => a.keep = true,
            "--wait" => a.wait = Some(number(value("seconds")?)?),
            "--seconds" => a.seconds = Some(number(value("seconds")?)?),
            "--timer" => a.timer = Some(value("off or seconds")?),
            "--expect" => a.expect = Some(number(value("a number")?)?),
            "--device-name" => a.device_name = Some(value("a name")?),
            "-h" | "--help" => bail!("{USAGE}"),
            other => bail!("unknown argument {other:?}\n{USAGE}"),
        }
    }
    Ok(a)
}

fn vars(key: &str) -> Option<String> {
    std::env::var(key).ok()
}

/// `cargo xtask smoke …`. Returns the exit status: 0, or 1 when a step failed.
pub fn smoke(args: &[String]) -> Result<i32> {
    let a = parse_args(args)?;
    match a.command.as_str() {
        "start" => start(&a),
        "step" => step(&a),
        "steps" => {
            list_steps();
            Ok(0)
        }
        "state" => state(&a),
        "report" => report(&a),
        "stop" => stop(&a),
        "backup" => backup(&a),
        "wipe" => wipe(&a),
        "rehearse" => rehearse(&a),
        "__supervise" => {
            let dir = a.place.resolve(None, &vars)?;
            supervise(&dir)?;
            Ok(0)
        }
        "help" | "-h" | "--help" => {
            println!("{USAGE}");
            Ok(0)
        }
        other => bail!("unknown smoke command {other:?}\n{USAGE}"),
    }
}

// ---- start ------------------------------------------------------------------------------------

/// The database schema versions of this checkout (the number of migrations per database).
fn build_schema() -> Value {
    let count = |dir: &str| {
        std::fs::read_dir(root().join(dir))
            .map(|d| {
                d.flatten()
                    .filter(|e| e.path().extension().is_some_and(|x| x == "sql"))
                    .count()
            })
            .unwrap_or(0)
    };
    json!({
        "core": count("crates/kourier-core/migrations"),
        "signal": count("crates/kourier-signal/migrations"),
    })
}

/// The commit of this checkout, with `+changes` when the tree has uncommitted ones.
fn build_commit() -> String {
    let git = |args: &[&str]| {
        Command::new("git")
            .arg("-C")
            .arg(root())
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
    };
    let head = git(&["rev-parse", "--short", "HEAD"]).unwrap_or_else(|| "unknown".to_owned());
    match git(&["status", "--porcelain"]) {
        Some(changes) if !changes.is_empty() => format!("{head}+changes"),
        _ => head,
    }
}

fn valid_tail(tail: &str) -> bool {
    (2..=15).contains(&tail.len()) && tail.bytes().all(|b| b.is_ascii_digit())
}

/// Whether the profile's lock is free (nobody supervises an app on it).
fn lock_is_free(dir: &ProfileDir) -> Result<bool> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(dir.lock_file())?;
    Ok(match file.try_lock() {
        Ok(()) => true,
        Err(std::fs::TryLockError::WouldBlock) => false,
        Err(std::fs::TryLockError::Error(e)) => return Err(e).context("lock the profile"),
    })
}

/// How long a lock that looks held is asked for again before `wipe` and `backup` take it for
/// the running app's.
const LOCK_SETTLE: Duration = Duration::from_secs(1);

/// Takes the exclusive lock of `file`, asking again for up to `wait` while it looks held.
///
/// Why it asks again: a lock lives as long as one copy of the descriptor does, and a thread
/// that starts a program forks the whole descriptor table, so a lock this process dropped a
/// moment ago is still carried by a child that has not reached its `exec` yet (the copy is
/// closed there). Such a child can be one of ANY thread of the process, for instance another
/// test's `Command::spawn`; its hold ends in milliseconds, the running app's does not. Without
/// the second ask, `wipe` and `backup` sometimes said "the app is running" about a profile
/// nothing used (the flake of `a_directory_that_is_no_smoke_profile_is_never_deleted`; in a stand-alone
/// stress test with four threads starting programs, 6,960 of 20,000 second locks were refused).
fn try_lock_for(file: &File, wait: Duration) -> Result<bool> {
    let deadline = Instant::now() + wait;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(true),
            Err(std::fs::TryLockError::WouldBlock) => {
                if Instant::now() >= deadline {
                    return Ok(false);
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(std::fs::TryLockError::Error(e)) => return Err(e).context("lock the profile"),
        }
    }
}

/// [`lock_is_free`] for the commands that delete or copy the profile: a lock that is only
/// carried by a child about to `exec` ([`try_lock_for`]) is free.
fn lock_is_free_settled(dir: &ProfileDir) -> Result<bool> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(dir.lock_file())?;
    try_lock_for(&file, LOCK_SETTLE)
}

fn tail_of(path: &Path, lines: usize) -> String {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

fn start(a: &Args) -> Result<i32> {
    start_in(a, &vars)
}

/// [`start`] with the environment variables that say where the profiles are.
fn start_in(a: &Args, vars: &dyn Fn(&str) -> Option<String>) -> Result<i32> {
    let env = a
        .env
        .clone()
        .ok_or_else(|| anyhow!("start needs --env <production|staging|local:<handshake.json>>"))?;
    let local = env.is_local();
    let dir = a.place.resolve(Some(&env), vars)?;
    // The user's go comes first: nothing below may contact a real server without it.
    if !local && !a.confirmed {
        bail!(
            "--env {} talks to Signal's real servers. That needs the user's go in this session: pass --confirmed-by-user only after the user said so. Nothing was started.",
            env.for_app()
        );
    }
    if !local && let Some(why) = unsafe_place_for_a_login(&dir.dir, vars) {
        bail!("refusing to keep a real login there: {why}. Use --profile <name>.");
    }
    let encryption = disk_encryption(&dir.dir);
    if !local && encryption.is_none() && !a.allow_unencrypted {
        bail!(
            "no disk encryption was found below {}. The profile's database key is a public constant (the development keyring), so on a plain disk the account's keys would lie there readable. Choose a place on the encrypted disk, or pass --allow-unencrypted-disk if the user accepts that.",
            dir.dir.display()
        );
    }
    // Everything that can be wrong with the command line is found before anything is made.
    let binding = env.binding()?;
    let now = now_ms();
    let mut state = match State::load(&dir)? {
        Some(state) => {
            let bound = state.str("env").unwrap_or("?");
            if bound != binding {
                bail!(
                    "this profile belongs to {bound}, and this start names {binding}. Refusing to start: that server would answer the profile's credentials with \"not registered\", and the app would wipe the login. Nothing was changed. Use the profile's own server, or another --profile."
                );
            }
            state
        }
        None => {
            if dir.dir.join("kourier").exists() {
                bail!(
                    "{} holds Kourier data but no smoke state file: it is not a profile of this tool, and it is not adopted",
                    dir.dir.display()
                );
            }
            State::new(&binding, &env.for_app(), DEFAULT_MAX_SENDS, now)
        }
    };
    let forgot_chats = forget_chats_of_another_db(&mut state, &dir.core_db());
    let tail = a
        .expect_tail
        .clone()
        .or_else(|| state.str("account_tail").map(str::to_owned));
    if let Some(tail) = &tail
        && !valid_tail(tail)
    {
        bail!(
            "--expect-tail takes 2 to 15 digits (the last digits of the test account's phone number)"
        );
    }
    if !local && !state.linked() && tail.is_none() {
        bail!(
            "a first link needs --expect-tail <last digits of the TEST account's phone number>. The app then refuses a QR code scanned by any other account before anything is registered (the personal phone is in the room as the peer, and nothing is ever linked to it)."
        );
    }
    private_dir(&dir.dir)?;
    private_dir(&dir.smoke())?;
    if !lock_is_free(&dir)? || Client::connect(&dir).is_ok() {
        bail!(
            "an app is already running on {} (one per profile). Use `step`, `state` or `stop`.",
            dir.dir.display()
        );
    }
    if !a.no_build {
        run(
            "build kourier (debug, Signal backend)",
            cargo().args(["build", "-p", "kourier", "--features", "signal"]),
        )?;
    }
    let bin = target_dir().join("debug/kourier");
    if !bin.is_file() {
        bail!("{} not found (run without --no-build)", bin.display());
    }
    let schema = build_schema();
    let has_data = dir.app_data().join("core.db").exists();
    if has_data
        && !state.0["schema"].is_null()
        && state.0["schema"] != schema
        && !a.allow_schema_change
    {
        bail!(
            "this build's database schema ({schema}) differs from the one this profile was last run with ({}). A newer build migrates the profile forward, and an older build then refuses it. Take a backup first (`cargo xtask smoke backup`), then start with --allow-schema-change.",
            state.0["schema"]
        );
    }
    state.set("schema", schema);
    state.set("build_commit", json!(build_commit()));
    state.set("env_arg", json!(env.for_app()));
    state.set("starts", json!(state.num("starts").unwrap_or(0) + 1));
    if let Some(max) = a.max_sends {
        state.set("max_sends", json!(max));
        state.set("sends_used", json!(0));
    }
    if let Some(tail) = &tail {
        state.set("account_tail", json!(tail));
    }
    state.save(&dir)?;
    let offscreen = a.offscreen.unwrap_or(local);
    write_private(
        &dir.launch_file(),
        &json!({
            "bin": bin,
            "issued_ms": now_ms(),
            "env_arg": env.for_app(),
            "local": local,
            "offscreen": offscreen,
            "desktop": a.desktop,
            "expect_tail": tail,
            "history_transfer": a.history_transfer,
        })
        .to_string(),
    )?;

    println!(
        "profile:  {} (mode 0700{})",
        dir.dir.display(),
        if dir.keeper { ", kept" } else { "" }
    );
    println!("server:   {binding}");
    println!(
        "at rest:  {}; the database key is the development keyring's public constant, so the databases' own encryption protects nothing here",
        match &encryption {
            Some(device) => format!("disk encryption ({device}) and the directory's mode"),
            None => "NO disk encryption found; only the directory's mode".to_owned(),
        }
    );
    println!(
        "log:      {} (redacted; safe to read)",
        dir.app_log().display()
    );
    for line in login_lines(&state, now) {
        println!("login:    {line}");
    }
    if forgot_chats {
        println!(
            "chats:    the app's database is not the file the peer chat and the group were remembered in (rebuilt or restored), and it may number its chats differently. They were forgotten: run `step receive` and `step group-receive` again before any sending step."
        );
    }
    if !local && !state.linked() {
        println!("\n{WHAT_LINKING_DOES}\n");
    }

    spawn_supervisor(&dir)?;
    let spawned = Instant::now();
    let deadline = spawned + Duration::from_secs(120);
    let client = loop {
        if let Ok(client) = Client::connect(&dir)
            && client.info().is_ok_and(|i| i["ready"] == json!(true))
        {
            break client;
        }
        // The supervisor gave up (it holds the lock for as long as it lives).
        let gone = spawned.elapsed() > Duration::from_secs(3)
            && lock_is_free(&dir).unwrap_or(false)
            && !dir.socket().exists();
        if Instant::now() >= deadline || gone {
            bail!(
                "the app did not come up.\n--- {} ---\n{}\n--- {} ---\n{}",
                dir.supervisor_log().display(),
                tail_of(&dir.supervisor_log(), 20),
                dir.app_log().display(),
                tail_of(&dir.app_log(), 40)
            );
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    let info = client.info()?;
    if info["dumpable"] == json!(true) {
        let _ = client.call(
            &json!({"op": "stop", "force": true}),
            Duration::from_secs(10),
        );
        bail!(
            "the app is dumpable although KOURIER_DEV_NO_DUMP=1 was set (an old build?): a crash would leave the account's keys in a core dump. It was stopped."
        );
    }
    client.wait("loaded", Duration::from_secs(90))?;
    println!(
        "started:  app pid {} ({}); a crash leaves no core dump",
        info["app_pid"],
        if offscreen {
            "offscreen"
        } else {
            "window on the desktop"
        }
    );
    if state.num("logged_out_ms").is_some() {
        println!(
            "next:     this profile's login has ended. Do not link again blindly: read {} for what the server answered. Linking again is done by hand in the window (\"Link again\"), on purpose only; `cargo xtask smoke wipe` removes the profile.",
            dir.app_log().display()
        );
        return Ok(0);
    }
    if !state.linked() {
        println!("next:     cargo xtask smoke step link");
        return Ok(0);
    }
    // A linked profile reconnects by itself: no QR code, no login.
    let started = Instant::now();
    match client.wait("account_active", Duration::from_secs(90)) {
        Ok(_) => {
            println!(
                "online:   reconnected without a QR code in {:.1} s",
                started.elapsed().as_secs_f32()
            );
            Ok(0)
        }
        Err(e) => {
            let observed = client
                .state()
                .map(|s| s["accounts"].to_string())
                .unwrap_or_default();
            println!(
                "NOT ONLINE after 90 s (accounts: {observed}): {e:#}\nDo not link again blindly. Read {} first: a 401 or 403 at connect means the server dropped the device; \"refusing to connect\" means the wrong server was named.",
                dir.app_log().display()
            );
            Ok(1)
        }
    }
}

/// Starts the supervisor in a session of its own: the app outlives the shell call that
/// started it, and a Ctrl-C in that shell does not reach it.
fn spawn_supervisor(dir: &ProfileDir) -> Result<()> {
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(dir.supervisor_log())?;
    let mut cmd = Command::new(std::env::current_exe().context("the xtask binary")?);
    cmd.args(["smoke", "__supervise", "--home"])
        .arg(&dir.dir)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    // SAFETY: `setsid` is async-signal-safe and only detaches the child from this session;
    // nothing else runs between fork and exec.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    cmd.spawn().context("start the smoke supervisor")?;
    Ok(())
}

// ---- stop, state, report, backup, wipe --------------------------------------------------------

fn stop(a: &Args) -> Result<i32> {
    let dir = a.place.resolve(None, &vars)?;
    let Ok(client) = Client::connect(&dir) else {
        println!("no app is running on {}", dir.dir.display());
        return Ok(0);
    };
    let pid = client.info()?["app_pid"].as_u64().unwrap_or(0);
    client.call(
        &json!({"op": "stop", "force": a.force}),
        Duration::from_secs(10),
    )?;
    let gone = |within: Duration| {
        let deadline = Instant::now() + within;
        while Instant::now() < deadline {
            if !dir.supervisor_file().exists() && lock_is_free(&dir).unwrap_or(false) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        false
    };
    if !gone(Duration::from_secs(60)) {
        if !a.force {
            bail!(
                "the app has not stopped after 60 s (pid {pid}). It was not killed. Read {}; `cargo xtask smoke stop --force` ends it with SIGTERM.",
                dir.app_log().display()
            );
        }
        if let Ok(pid) = libc::pid_t::try_from(pid)
            && pid > 1
        {
            // SAFETY: a signal to the one process this tool started.
            unsafe { libc::kill(pid, libc::SIGTERM) };
        }
        if !gone(Duration::from_secs(20)) {
            bail!("the app (pid {pid}) did not end after SIGTERM");
        }
    }
    println!("stopped. Only the app was stopped: the login is kept.");
    if let Some(state) = State::load(&dir)? {
        for line in login_lines(&state, now_ms()) {
            println!("login:    {line}");
        }
    }
    // Plaintext playback copies live in the profile's run directory under KOURIER_HOME.
    let media = dir.dir.join("kourier/run/media");
    let left = walk_files(&media);
    if left > 0 {
        println!(
            "NOTE: {left} file(s) are left in {} (plaintext media copies)",
            media.display()
        );
    }
    Ok(0)
}

/// How many regular files are below `dir`.
fn walk_files(dir: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => walk_files(&e.path()),
            Ok(t) if t.is_file() => 1,
            _ => 0,
        })
        .sum()
}

/// The recorded steps (`smoke/steps.jsonl`), oldest first.
fn recorded_steps(dir: &ProfileDir) -> Vec<Value> {
    std::fs::read_to_string(dir.steps_log())
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

fn state(a: &Args) -> Result<i32> {
    let dir = a.place.resolve(None, &vars)?;
    let Some(state) = State::load(&dir)? else {
        println!(
            "{} is not a smoke profile yet (no start so far)",
            dir.dir.display()
        );
        return Ok(0);
    };
    println!("profile:  {}", dir.dir.display());
    println!("server:   {}", state.str("env").unwrap_or("?"));
    println!(
        "build:    {} (schema {})",
        state.str("build_commit").unwrap_or("?"),
        state.0["schema"]
    );
    for line in login_lines(&state, now_ms()) {
        println!("login:    {line}");
    }
    println!(
        "sends:    {} of {} used",
        state.num("sends_used").unwrap_or(0),
        state.num("max_sends").unwrap_or(DEFAULT_MAX_SENDS)
    );
    let conv = |key: &str| {
        state.0[key].as_i64().map_or_else(
            || "not known yet".to_owned(),
            |id| format!("conversation #{id}"),
        )
    };
    println!(
        "chats:    peer: {} (timer {}); group: {}",
        conv("peer_conv"),
        state.0["peer_timer_secs"].as_i64().map_or_else(
            || "not compared with the phone yet".to_owned(),
            |secs| format!("confirmed {}", timer_text(secs))
        ),
        conv("group_conv")
    );
    let steps = recorded_steps(&dir);
    let passed = steps.iter().filter(|s| s["result"] == "PASS").count();
    println!(
        "steps:    {} run, {passed} passed, {} failed",
        steps.len(),
        steps.len() - passed
    );
    let Ok(client) = Client::connect(&dir) else {
        println!("app:      not running");
        return Ok(0);
    };
    let info = client.info()?;
    println!(
        "app:      running since {} (pid {}), {}",
        utc(info["started_ms"].as_u64().unwrap_or(0)),
        info["app_pid"],
        if info["online"] == json!(true) {
            "online"
        } else {
            "not online"
        }
    );
    let ui = client.state()?;
    println!(
        "ui:       accounts {}, {} conversations, open #{}, banner {}, challenge {}, login {}",
        ui["accounts"], ui["conversations"], ui["open"], ui["banner"], ui["challenge"], ui["login"]
    );
    let chats = parse_chats(&client.ctl("chats", json!({}), Duration::from_secs(30))?);
    let of = |kind: &str| chats.iter().filter(|c| c.kind == kind).count();
    println!(
        "list:     {} direct, {} group, {} unread in all",
        of("direct"),
        of("group"),
        chats.iter().map(|c| c.unread).sum::<i64>()
    );
    Ok(0)
}

fn took(step: &Value) -> String {
    let ms = step["ended_ms"]
        .as_u64()
        .unwrap_or(0)
        .saturating_sub(step["started_ms"].as_u64().unwrap_or(0));
    format!("{}.{} s", ms / 1000, ms % 1000 / 100)
}

/// The Markdown report of a profile's steps.
fn report_text(dir: &ProfileDir, state: &State, steps: &[Value], now: u64) -> String {
    let mut out = String::from("# Kourier smoke run\n\n");
    out.push_str(&format!("- Profile: `{}`\n", dir.dir.display()));
    out.push_str(&format!("- Server: {}\n", state.str("env").unwrap_or("?")));
    out.push_str(&format!(
        "- Build: {} (schema {})\n",
        state.str("build_commit").unwrap_or("?"),
        state.0["schema"]
    ));
    for line in login_lines(state, now) {
        out.push_str(&format!("- Login: {}\n", line.replace('\n', " ")));
    }
    out.push_str(&format!(
        "- Messages sent by the tool: {} of {}\n",
        state.num("sends_used").unwrap_or(0),
        state.num("max_sends").unwrap_or(DEFAULT_MAX_SENDS)
    ));
    let passed = steps.iter().filter(|s| s["result"] == "PASS").count();
    out.push_str(&format!(
        "- Steps: {} run, {passed} passed, {} failed\n- Written: {}\n",
        steps.len(),
        steps.len() - passed,
        utc(now)
    ));
    out.push_str("\n## Steps in the order they ran\n\n| # | Step | Result | Started | Took | Sent | Reason |\n|---|---|---|---|---|---|---|\n");
    for (i, s) in steps.iter().enumerate() {
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} | {} |\n",
            i + 1,
            s["name"].as_str().unwrap_or("?"),
            s["result"].as_str().unwrap_or("?"),
            utc(s["started_ms"].as_u64().unwrap_or(0)),
            took(s),
            s["sends"].as_u64().unwrap_or(0),
            s["reason"]
                .as_str()
                .unwrap_or("")
                .replace('|', "/")
                .replace('\n', " "),
        ));
    }
    out.push_str("\n## Latest result of each step\n\n| Step | Result | Runs |\n|---|---|---|\n");
    for def in STEPS {
        let runs: Vec<&Value> = steps.iter().filter(|s| s["name"] == def.name).collect();
        let latest = runs
            .last()
            .map_or("not run", |s| s["result"].as_str().unwrap_or("?"));
        out.push_str(&format!("| {} | {latest} | {} |\n", def.name, runs.len()));
    }
    out.push_str("\n## What each step saw\n");
    for (i, s) in steps.iter().enumerate() {
        out.push_str(&format!(
            "\n### {}. {} ({})\n\n",
            i + 1,
            s["name"].as_str().unwrap_or("?"),
            s["result"].as_str().unwrap_or("?")
        ));
        for note in s["notes"].as_array().into_iter().flatten() {
            out.push_str(&format!("- {}\n", note.as_str().unwrap_or("")));
        }
        out.push_str(&format!(
            "- Log: bytes {} to {} of `smoke/app.log`\n",
            s["log"]["from"], s["log"]["to"]
        ));
    }
    out
}

fn report(a: &Args) -> Result<i32> {
    let dir = a.place.resolve(None, &vars)?;
    let state = State::load(&dir)?.context("not a smoke profile (no state file)")?;
    let steps = recorded_steps(&dir);
    write_private(&dir.report(), &report_text(&dir, &state, &steps, now_ms()))?;
    let passed = steps.iter().filter(|s| s["result"] == "PASS").count();
    println!(
        "report:   {} ({} steps, {passed} passed, {} failed)",
        dir.report().display(),
        steps.len(),
        steps.len() - passed
    );
    Ok(0)
}

/// Copies a directory tree: directories 0700, files with their modes; sockets, links and the
/// app's run directory are left out.
fn copy_tree(from: &Path, to: &Path) -> Result<usize> {
    private_dir(to)?;
    let mut copied = 0;
    for entry in std::fs::read_dir(from).with_context(|| format!("read {}", from.display()))? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let target = to.join(entry.file_name());
        if kind.is_dir() {
            if entry.file_name() == "run" {
                continue;
            }
            copied += copy_tree(&entry.path(), &target)?;
        } else if kind.is_file() {
            std::fs::copy(entry.path(), &target)
                .with_context(|| format!("copy {}", entry.path().display()))?;
            copied += 1;
        }
    }
    Ok(copied)
}

fn backup(a: &Args) -> Result<i32> {
    let dir = a.place.resolve(None, &vars)?;
    let state = State::load(&dir)?.context("not a smoke profile (no state file)")?;
    // The app must not write while its databases are copied.
    let lock_file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(dir.lock_file())?;
    if !try_lock_for(&lock_file, LOCK_SETTLE)? {
        bail!("the app is running on this profile: `cargo xtask smoke stop` first");
    }
    let name = dir
        .dir
        .file_name()
        .context("profile directory name")?
        .to_string_lossy();
    let parent = match &a.to {
        Some(to) => std::path::absolute(to)?,
        None => dir
            .dir
            .parent()
            .context("profile directory parent")?
            .to_path_buf(),
    };
    let dest = parent.join(format!("{name}.backup-{}", utc_compact(now_ms())));
    if dest.exists() {
        bail!("{} exists already", dest.display());
    }
    let real = !state.str("env").unwrap_or("").starts_with("local:");
    if real {
        if let Some(why) = unsafe_place_for_a_login(&dest, &vars) {
            bail!("refusing to put a copy of a real login there: {why}");
        }
        if disk_encryption(&parent).is_none() && !a.allow_unencrypted {
            bail!(
                "no disk encryption was found below {}: the copy holds the account's keys under a public database key. Choose a place on the encrypted disk (or --allow-unencrypted-disk).",
                parent.display()
            );
        }
    }
    let copied = copy_tree(&dir.dir, &dest)?;
    drop(lock_file);
    println!(
        "backup:   {} ({copied} files). It holds the test account's keys and is as sensitive as the profile itself. Restore only a copy taken after the last run, as a whole directory, to the same path.",
        dest.display()
    );
    Ok(0)
}

fn wipe(a: &Args) -> Result<i32> {
    let dir = a.place.resolve(None, &vars)?;
    let state = State::load(&dir)?.with_context(|| {
        format!(
            "{} is not a smoke profile (no state file): nothing is deleted",
            dir.dir.display()
        )
    })?;
    println!(
        "wipe:     {} (server {}). This deletes the login for good.",
        dir.dir.display(),
        state.str("env").unwrap_or("?")
    );
    for line in login_lines(&state, now_ms()) {
        println!("login:    {line}");
    }
    if !a.yes_delete {
        println!(
            "Nothing was deleted. If the device is still linked, unlink it on the phone first (Settings > Linked devices). Then run this again with --yes-delete-the-login. Backups made with `backup` are separate directories and stay."
        );
        return Ok(1);
    }
    if !lock_is_free_settled(&dir)? {
        bail!("the app is running on this profile: `cargo xtask smoke stop` first");
    }
    std::fs::remove_dir_all(&dir.dir).with_context(|| format!("delete {}", dir.dir.display()))?;
    println!("deleted.");
    Ok(0)
}

// ---- Steps ------------------------------------------------------------------------------------

/// One row of the conversation list, as dev-control `chats` reports it (no title).
#[derive(Clone, Debug, PartialEq, Eq)]
struct Chat {
    id: i64,
    kind: String,
    is_self: bool,
    unread: i64,
}

fn parse_chats(v: &Value) -> Vec<Chat> {
    v["chats"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| Chat {
            id: c["conversation"].as_i64().unwrap_or(0),
            kind: c["kind"].as_str().unwrap_or("other").to_owned(),
            is_self: c["self"] == json!(true),
            unread: c["unread"].as_i64().unwrap_or(0),
        })
        .collect()
}

/// What the app shows at one moment: the list, and the rows of the open chat.
#[derive(Clone, Debug, Default, PartialEq)]
struct Snap {
    chats: Vec<Chat>,
    /// The open conversation (0: none).
    open: i64,
    /// The open conversation's message rows, newest first (dev-control `state.rows`).
    rows: Vec<Value>,
}

impl Snap {
    fn take(client: &Client) -> Result<Self> {
        let chats = parse_chats(&client.ctl("chats", json!({}), Duration::from_secs(30))?);
        let state = client.state()?;
        Ok(Self {
            chats,
            open: state["open"].as_i64().unwrap_or(0),
            rows: state["rows"].as_array().cloned().unwrap_or_default(),
        })
    }
}

/// The message rows of the open chat in a state (dev-control `state.rows`), newest first.
fn rows_in(state: &Value) -> &[Value] {
    state["rows"].as_array().map_or(&[], Vec::as_slice)
}

/// The highest message id among `rows` (ids grow with every stored message).
fn newest_message(rows: &[Value]) -> i64 {
    rows.iter()
        .filter_map(|r| r["message"].as_i64())
        .max()
        .unwrap_or(0)
}

/// The lowest message id among `rows` (0: there is none).
fn oldest_message(rows: &[Value]) -> i64 {
    rows.iter()
        .filter_map(|r| r["message"].as_i64())
        .min()
        .unwrap_or(0)
}

/// Whether a message row shows content: a text or an attachment, and not a tombstone. A
/// placeholder has neither ("could not be decrypted", or a kind of message Kourier does not
/// show), so it stands for a message that did not arrive.
fn has_content(row: &Value) -> bool {
    row["deleted"] != json!(true)
        && (row["text"] == json!(true) || row["attachments"].as_u64().is_some_and(|n| n > 0))
}

/// The newest message from others in `rows` (newest first).
fn newest_from_others(rows: &[Value]) -> Option<&Value> {
    rows.iter().find(|r| r["own"] != json!(true))
}

/// Whether the newest message from others in `rows` (newest first) is a placeholder: it
/// shows no content and was not deleted. `receive` counts by a chat's unread counter, which
/// such a row raises like a message that arrived.
fn placeholder_on_top(rows: &[Value]) -> bool {
    newest_from_others(rows).is_some_and(|r| r["deleted"] != json!(true) && !has_content(r))
}

/// How many messages from others arrived per chat of one of `kinds` since the baseline: for
/// the chat that is open then and now, the rows newer than the baseline's newest one (an open
/// chat is read at once, so its unread count stays 0); for every other chat, the growth of
/// its unread count. Chats without a new message are left out.
fn incoming(
    base_chats: &[Chat],
    base_open: i64,
    base_newest: i64,
    now: &Snap,
    kinds: &[&str],
) -> Vec<(i64, i64)> {
    now.chats
        .iter()
        .filter(|c| kinds.contains(&c.kind.as_str()) && !c.is_self)
        .filter_map(|c| {
            let count = if c.id == now.open && c.id == base_open {
                let new = now
                    .rows
                    .iter()
                    .filter(|r| r["own"] != json!(true))
                    .filter(|r| r["message"].as_i64().is_some_and(|m| m > base_newest))
                    .count();
                i64::try_from(new).unwrap_or(i64::MAX)
            } else {
                let before = base_chats
                    .iter()
                    .find(|b| b.id == c.id)
                    .map_or(0, |b| b.unread);
                c.unread - before
            };
            (count > 0).then_some((c.id, count))
        })
        .collect()
}

/// How many rows a freshly opened chat loads, system rows included (the app's
/// `TIMELINE_PAGE`). One look at a chat shows no more: dev-control cannot page back.
const TIMELINE_PAGE: usize = 50;
/// From this many message rows on, one look at a chat is taken for a whole page: dev-control
/// lists message rows only, and system rows (timer and group changes) take places of the
/// page too. A page with more than ten system rows is not recognised as one.
const WHOLE_PAGE: usize = TIMELINE_PAGE - 10;

/// What one chat holds from others that was stored after the baseline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Stored {
    chat: i64,
    /// Messages with content: these arrived.
    arrived: i64,
    /// Rows without content ([`has_content`]): a placeholder or a tombstone, never an arrived
    /// message.
    empty: i64,
    /// The chat may hold more new rows than the look shows: not one of its rows is as old as
    /// the baseline, and they may be a whole page ([`WHOLE_PAGE`]). The counts are a lower
    /// bound then.
    cut: bool,
}

/// What each chat holds from others that was stored after message `after` (ids grow with
/// every stored message, whatever its chat). Chats with nothing to say are left out.
///
/// This counts rows and not unread counters: those fall as soon as someone opens a chat in
/// the window or reads it on the account's phone, which made the first real run's catch-up
/// report 0 of 3 (2026-10-07).
fn stored_after(chats: &[(i64, Vec<Value>)], after: i64) -> Vec<Stored> {
    let count = |n: usize| i64::try_from(n).unwrap_or(i64::MAX);
    chats
        .iter()
        .filter_map(|(id, rows)| {
            let theirs: Vec<&Value> = rows
                .iter()
                .filter(|r| r["own"] != json!(true))
                .filter(|r| r["message"].as_i64().is_some_and(|m| m > after))
                .collect();
            let arrived = theirs.iter().filter(|r| has_content(r)).count();
            // A row from before the baseline proves that the look shows everything newer.
            let reaches_back = rows
                .iter()
                .any(|r| r["message"].as_i64().is_some_and(|m| m <= after));
            let stored = Stored {
                chat: *id,
                arrived: count(arrived),
                empty: count(theirs.len() - arrived),
                cut: !reaches_back && rows.len() >= WHOLE_PAGE,
            };
            (stored.cut || !theirs.is_empty()).then_some(stored)
        })
        .collect()
}

/// `(conversation, count)` of every chat where `count` is not zero, for a step's lines.
fn per_chat(new: &[Stored], count: impl Fn(&Stored) -> i64) -> Vec<(i64, i64)> {
    new.iter()
        .map(|s| (s.chat, count(s)))
        .filter(|(_, n)| *n > 0)
        .collect()
}

/// Fails when `new` is no count: a chat that may hold more new rows than one look shows
/// ([`Stored::cut`]) makes every number a lower bound, and a number that may be short is
/// never reported as one.
fn whole_count(new: &[Stored]) -> Result<()> {
    if let Some(cut) = new.iter().find(|s| s.cut) {
        bail!(
            "conversation #{} shows only rows that are newer than the baseline, and as many as one look at a chat can show (a page of {TIMELINE_PAGE} rows, system rows included): it may hold more new messages than the tool can count. Nothing was counted. Count this catch-up by eye in the window",
            cut.chat
        );
    }
    Ok(())
}

/// The verdict on the last count of a catch-up that expects `expect` messages: the arrived
/// messages as `(conversation, count)`, or why the step fails. A new row without content
/// fails it whatever the numbers say: it stands for a message that did not arrive.
fn caught_up(new: &[Stored], expect: i64) -> Result<Vec<(i64, i64)>> {
    whole_count(new)?;
    let by_chat = per_chat(new, |s| s.arrived);
    let got: i64 = new.iter().map(|s| s.arrived).sum();
    let empty: i64 = new.iter().map(|s| s.empty).sum();
    if empty > 0 {
        bail!(
            "new rows from others that show no content: {empty}, {:?} (conversation, count). Such a row is a placeholder (a message that could not be decrypted, or a kind of message Kourier does not show) or a deleted message, and not an arrived message. With content: {got} of the {expect} expected, {by_chat:?}. Read the app's log from the start on for a decryption error or a retry job: the lines below cover this step only",
            per_chat(new, |s| s.empty)
        );
    }
    if got != expect {
        bail!(
            "expected {expect} new messages after the catch-up, found {got}: {by_chat:?} (conversation, count).{}",
            if got > expect {
                " More than expected: a message sent after the start or by someone else, or a message that was stored twice"
            } else {
                " Fewer than expected: a message did not arrive, or its chat's disappearing timer erased it after it was read (in the window since the start, on the test phone, or by an earlier look of this step)"
            }
        );
    }
    Ok(by_chat)
}

/// Calls `f` every `every` until it returns a value or `timeout` passes.
fn poll<T>(
    timeout: Duration,
    every: Duration,
    mut f: impl FnMut() -> Result<Option<T>>,
) -> Result<Option<T>> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(v) = f()? {
            return Ok(Some(v));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        std::thread::sleep(every);
    }
}

/// One running step.
struct StepRun<'a> {
    client: &'a Client,
    dir: &'a ProfileDir,
    args: &'a Args,
    notes: Vec<String>,
    sends: u64,
    /// Where the app's log stood when the step began.
    log_from: u64,
}

impl StepRun<'_> {
    fn note(&mut self, text: impl Into<String>) {
        self.notes.push(text.into());
    }

    /// What the user has to do on a phone.
    fn phone(&self, what: &str) {
        println!("PHONE: {what}");
    }

    /// The step now waits for the user (a script that plays the phone acts on this line).
    fn waiting(&self, what: &str) {
        println!("WAIT: {what}");
        let _ = std::io::stdout().flush();
    }

    fn wait_secs(&self, default: u64) -> Duration {
        Duration::from_secs(self.args.wait.unwrap_or(default))
    }

    fn tool_state(&self) -> Result<State> {
        Ok(State(self.client.info()?["state"].clone()))
    }

    /// The remembered conversation of `key` (`peer_conv`, `group_conv`).
    fn remembered(&self, key: &str, set_by: &str) -> Result<i64> {
        let id = self.tool_state()?.0[key]
            .as_i64()
            .filter(|id| *id > 0)
            .ok_or_else(|| anyhow!("no {key} is known yet: run `step {set_by}` first"))?;
        let chats = parse_chats(
            &self
                .client
                .ctl("chats", json!({}), Duration::from_secs(30))?,
        );
        let kind = if key == "group_conv" {
            "group"
        } else {
            "direct"
        };
        if !still_the_chat(&chats, id, kind) {
            bail!(
                "conversation #{id}, remembered as {key}, is not a {kind} chat of the list any more: nothing was done. Run `step {set_by}` again"
            );
        }
        Ok(id)
    }

    /// Opens a conversation by its id (this marks it read, as a click does) and waits until
    /// its header is loaded: only then does the state say anything about the conversation
    /// (its timer above all, which reads as `null` and never as "off" until then).
    fn open(&self, id: i64) -> Result<Value> {
        self.client.ctl(
            "open_id",
            json!({"conversation": id}),
            Duration::from_secs(30),
        )?;
        let opened = poll(Duration::from_secs(15), Duration::from_millis(200), || {
            let state = self.client.state()?;
            Ok(
                (state["open"].as_i64() == Some(id) && chat_timer(&state).is_some())
                    .then_some(state),
            )
        })?;
        if let Some(state) = opened {
            return Ok(state);
        }
        let other = self.client.state()?["open"]
            .as_i64()
            .filter(|open| *open > 0 && *open != id);
        bail!(
            "conversation #{id} did not open (or its header did not load){}",
            other.map_or_else(String::new, |other| format!(
                ": conversation #{other} is the open one. Was a chat clicked in the window meanwhile?"
            ))
        )
    }

    /// Opens a conversation and returns the app's state once two looks in a row show the
    /// same message rows. The rows are loaded apart from the header and a moment after it,
    /// so the state [`Self::open`] returns may show none of them yet: whatever a step
    /// compares later looks against (a base) comes from here.
    fn settled(&self, id: i64) -> Result<Value> {
        let mut seen = self.open(id)?;
        // The rows get their time from the moment the header is there, however long that took.
        let opened = Instant::now();
        for _ in 0..40 {
            std::thread::sleep(Duration::from_millis(250));
            let state = self.client.state()?;
            if state["open"].as_i64() != Some(id) {
                bail!(
                    "conversation #{id} was left while its rows were read (a click on a chat in the window?): keep the hands off the window while the step reads, and run it again"
                );
            }
            let (now, before) = (rows_in(&state), rows_in(&seen));
            let same = now.len() == before.len() && newest_message(now) == newest_message(before);
            // An empty chat is believed only after a longer look.
            let long_enough =
                opened.elapsed() >= Duration::from_millis(if now.is_empty() { 1500 } else { 600 });
            if same && long_enough {
                return Ok(state);
            }
            seen = state;
        }
        bail!("the rows of conversation #{id} kept changing: is someone sending right now?")
    }

    /// Opens a conversation and returns its message rows, newest first, once they have
    /// settled ([`Self::settled`]). That is one page of a freshly opened chat
    /// ([`TIMELINE_PAGE`] rows, system rows included), not its whole history.
    fn rows_of(&self, id: i64) -> Result<Vec<Value>> {
        Ok(rows_in(&self.settled(id)?).to_vec())
    }

    /// The app's state while conversation `id` is the open one, and `None` while another
    /// chat is (someone clicked one in the window). A step that watches one chat during a
    /// wait looks through this: the rows of another chat are never taken for its own.
    fn state_of(&self, id: i64) -> Result<Option<Value>> {
        let state = self.client.state()?;
        Ok((state["open"].as_i64() == Some(id)).then_some(state))
    }

    /// [`Self::state_of`] for one round of a wait on conversation `id`. The first round that
    /// finds another chat open asks for the watched one (`asked` remembers that it did): its
    /// rows and pictures show only while it is the open chat, and the step does not pull
    /// the window back from a chat someone is looking at.
    fn watched(&self, id: i64, asked: &mut bool) -> Result<Option<Value>> {
        let state = self.state_of(id)?;
        if state.is_none() && !*asked {
            *asked = true;
            self.phone("on this computer: another chat is open in the Kourier window. Open the chat with the personal account again: the step sees what arrives only while that chat is the open one");
        }
        Ok(state)
    }

    /// What a wait on conversation `id` adds to its failure when another chat is the open
    /// one at its end.
    fn elsewhere(&self, id: i64) -> Result<String> {
        Ok(match self.state_of(id)? {
            Some(_) => String::new(),
            None => format!(
                " Conversation #{id} is not the open chat any more: the step watches it only while it is open in the window. Leave the chat list alone while a step waits"
            ),
        })
    }

    /// The message rows of every listed chat of one of `kinds` (never the note to self), by
    /// conversation. Each chat is opened in turn, as a click would ([`visit_order`]), and the
    /// chat that was open before is the open one again at the end. In a focused window every
    /// opened chat is marked read.
    fn rows_by_chat(&self, kinds: &[&str]) -> Result<Vec<(i64, Vec<Value>)>> {
        let was_open = self.client.state()?["open"].as_i64().unwrap_or(0);
        let chats = parse_chats(
            &self
                .client
                .ctl("chats", json!({}), Duration::from_secs(30))?,
        );
        let mut out = Vec::new();
        let mut last = was_open;
        for chat in visit_order(&chats, kinds, was_open) {
            out.push((chat, self.rows_of(chat)?));
            last = chat;
        }
        if was_open > 0 && last != was_open {
            // Best effort: the chat may have left the list.
            let _ = self.open(was_open);
        }
        Ok(out)
    }

    /// Opens the peer chat for a send from Kourier and returns its id, or fails when its
    /// disappearing-messages timer is not the one the user confirmed from the PERSONAL phone.
    ///
    /// Every 1:1 message restates the chat's timer, and the other side takes it over. So
    /// nothing goes to the peer unless Kourier's value was compared with the phone's: `--timer`
    /// on this very step, or the value `step send --timer` confirmed earlier. A timer that
    /// changed since then (or was never confirmed) refuses the step; nothing is sent.
    fn peer_for_send(&mut self) -> Result<i64> {
        let peer = self.remembered("peer_conv", "receive")?;
        let given = self.args.timer.as_deref().map(parse_timer).transpose()?;
        let confirmed = self.tool_state()?.0["peer_timer_secs"].as_i64();
        let state = self.open(peer)?;
        let shown = timer_to_send_with(chat_timer(&state), given, confirmed)?;
        if given.is_some() && confirmed != Some(shown) {
            self.client.call(
                &json!({"op": "set", "peer_timer_secs": shown}),
                Duration::from_secs(5),
            )?;
        }
        self.note(format!(
            "the chat's timer in Kourier is {}, as confirmed from the phone",
            timer_text(shown)
        ));
        Ok(peer)
    }

    /// Fails when a send must not go out now ([`send_blocker`]).
    fn ready_to_send(&self) -> Result<()> {
        match send_blocker(&self.client.state()?) {
            Some(why) => bail!("{why}"),
            None => Ok(()),
        }
    }

    /// Sends one text from the open conversation's composer and waits until it is sent.
    fn send_text(&mut self, what: &str) -> Result<()> {
        self.ready_to_send()?;
        let text = format!("Kourier smoke test: {what}, {}", utc(now_ms()));
        self.sends += 1;
        self.client
            .ctl_sending("send", json!({"text": text}), 1, Duration::from_secs(60))?;
        self.sent("the message")
    }

    /// Waits until the newest own message of the open conversation is sent.
    fn sent(&mut self, what: &str) -> Result<()> {
        match self
            .client
            .wait("outgoing_state:Sent", Duration::from_secs(60))
        {
            Ok(_) => Ok(()),
            Err(e) => {
                let state = self.client.state()?;
                if !state["challenge"].is_null() {
                    bail!("{what} is held: the server asks for a challenge (428). Stop sending");
                }
                bail!(
                    "{what} was not sent within 60 s (newest own message: {}; banner: {}): {e:#}",
                    state["newest_own"],
                    state["banner"]
                )
            }
        }
    }

    /// The newest own row that is not deleted.
    fn newest_own(&self) -> Result<Option<Value>> {
        let state = self.client.state()?;
        Ok(state["rows"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|r| r["own"] == json!(true) && r["deleted"] != json!(true))
            .cloned())
    }
}

/// The listed chats of one of `kinds` (never the note to self) in the order their rows are
/// read in: the list's order, with the chat that is open now (`open`, 0: none) last.
///
/// Only a switch to a chat loads its newest page. The window of the chat that is open may
/// stand elsewhere (after a jump to a quoted message, or scrolled far up), and a look at it
/// as it stands would miss its newest rows. Read last, it is opened anew like the others. A
/// single chat that is the open one is read as it stands: there is nothing to switch from.
fn visit_order(chats: &[Chat], kinds: &[&str], open: i64) -> Vec<i64> {
    let (last, mut order): (Vec<i64>, Vec<i64>) = chats
        .iter()
        .filter(|c| kinds.contains(&c.kind.as_str()) && !c.is_self)
        .map(|c| c.id)
        .partition(|id| *id == open);
    order.extend(last);
    order
}

/// Whether the list still has conversation `id` as a chat of `kind` (and not the note to
/// self): what a remembered chat must be before anything is opened or sent there.
fn still_the_chat(chats: &[Chat], id: i64, kind: &str) -> bool {
    chats
        .iter()
        .any(|c| c.id == id && c.kind == kind && !c.is_self)
}

/// The open chat's disappearing-messages timer in seconds (0: off) as Kourier knows it, and
/// `None` while its header is not loaded (dev-control `state.timer_secs` is `null` then).
fn chat_timer(state: &Value) -> Option<i64> {
    state["timer_secs"].as_i64().filter(|secs| *secs >= 0)
}

/// How many files wait in the open chat's composer (`None`: the state does not say).
fn composer_files(state: &Value) -> Option<u64> {
    state["composer"]["files"].as_u64()
}

/// Why nothing may be sent in the app's state `state` (dev-control `state`), if anything:
/// the account waits for a challenge, it is not online, or the open chat's composer still
/// holds files. Those would ride along with the next send, which would then send more than
/// its step says (a step that attached files and failed before its send leaves them there).
fn send_blocker(state: &Value) -> Option<String> {
    if !state["challenge"].is_null() {
        return Some(
            "the server asks for a challenge (428): stop sending. The user solves the captcha in a browser; do not let sends run against a standing challenge"
                .to_owned(),
        );
    }
    if state["accounts"] != json!(["online"]) {
        return Some(format!(
            "the account is not online (accounts: {})",
            state["accounts"]
        ));
    }
    match composer_files(state) {
        Some(0) => None,
        left => Some(format!(
            "the composer of the open chat holds {} file(s) an earlier step left there: nothing was sent. Remove them in the window (the x on each file) and run the step again",
            left.map_or_else(|| "an unknown number of".to_owned(), |n| n.to_string())
        )),
    }
}

/// The timer a 1:1 send would restate, when it may go out: Kourier's value for the chat
/// (`shown`) must be known and equal the phone's, which is `given` (`--timer` on this step)
/// or else `confirmed` (what an earlier step compared). Everything else is an error that
/// says why nothing is sent.
fn timer_to_send_with(
    shown: Option<i64>,
    given: Option<i64>,
    confirmed: Option<i64>,
) -> Result<i64> {
    let Some(shown) = shown else {
        bail!(
            "Kourier does not show this chat's disappearing-messages timer yet: nothing was sent"
        );
    };
    match (given, confirmed) {
        (Some(phone), _) if phone != shown => bail!(
            "Kourier shows the timer of this chat as {}, the phone shows {}: nothing was sent. A send would set the peer's chat to Kourier's value",
            timer_text(shown),
            timer_text(phone)
        ),
        (Some(_), _) => Ok(shown),
        (None, Some(confirmed)) if confirmed != shown => bail!(
            "the timer of this chat is {} in Kourier now, and {} was confirmed from the phone: nothing was sent. Compare with the PERSONAL phone again and pass --timer <off|seconds>",
            timer_text(shown),
            timer_text(confirmed)
        ),
        (None, Some(_)) => Ok(shown),
        (None, None) => bail!(
            "this chat's disappearing-messages timer was not compared with the PERSONAL phone yet: nothing was sent. Run `step send --timer <off|seconds>` first, or pass --timer here. Every 1:1 message restates the chat's timer, and a wrong value would change the setting on the personal account"
        ),
    }
}

fn timer_text(secs: i64) -> String {
    if secs == 0 {
        "off".to_owned()
    } else {
        format!("{secs} s")
    }
}

fn state_is_sent(row: &Value) -> bool {
    matches!(row["state"].as_str(), Some("sent" | "delivered" | "read"))
}

type StepFn = fn(&mut StepRun<'_>) -> Result<()>;

/// A named step of the run.
struct StepDef {
    name: &'static str,
    /// The most messages the step sends.
    sends: u64,
    /// Ends the kept login: only with `--destroys-login`.
    destroys_login: bool,
    what: &'static str,
    run: StepFn,
}

/// The steps, in the order of a run (`docs/smoke-run.md`).
const STEPS: &[StepDef] = &[
    StepDef {
        name: "link",
        sends: 0,
        destroys_login: false,
        run: step_link,
        what: "first contact: the QR code, the link, keys, capabilities, the first sync",
    },
    StepDef {
        name: "passive",
        sends: 0,
        destroys_login: false,
        run: step_passive,
        what: "stay connected and send nothing (--seconds, default 300)",
    },
    StepDef {
        name: "reconnect",
        sends: 0,
        destroys_login: false,
        run: step_reconnect,
        what: "after a stop and a start: online again without a QR code",
    },
    StepDef {
        name: "receive",
        sends: 0,
        destroys_login: false,
        run: step_receive,
        what: "one text from the personal account arrives once, in a 1:1 chat",
    },
    StepDef {
        name: "read",
        sends: 0,
        destroys_login: false,
        run: step_read,
        what: "open that chat: its newest message from others is no placeholder, and the chat is marked read (read receipts and a read sync go out)",
    },
    StepDef {
        name: "send",
        sends: 1,
        destroys_login: false,
        run: step_send,
        what: "one text to the personal account (--timer <off|seconds>: what the phone shows for this chat)",
    },
    StepDef {
        name: "group-receive",
        sends: 0,
        destroys_login: false,
        run: step_group_receive,
        what: "one text in the group from the personal account arrives once",
    },
    StepDef {
        name: "group-send",
        sends: 1,
        destroys_login: false,
        run: step_group_send,
        what: "one text to the group",
    },
    StepDef {
        name: "attachment-receive",
        sends: 0,
        destroys_login: false,
        run: step_attachment_receive,
        what: "a photo from the personal account arrives and is shown",
    },
    StepDef {
        name: "attachment-send",
        sends: 2,
        destroys_login: false,
        run: step_attachment_send,
        what: "a small picture and a small text file to the personal account",
    },
    StepDef {
        name: "react",
        sends: 1,
        destroys_login: false,
        run: step_react,
        what: "a reaction to the peer's newest message",
    },
    StepDef {
        name: "reply",
        sends: 1,
        destroys_login: false,
        run: step_reply,
        what: "a reply that quotes the peer's newest message",
    },
    StepDef {
        name: "edit",
        sends: 2,
        destroys_login: false,
        run: step_edit,
        what: "a text, then its edit",
    },
    StepDef {
        name: "delete",
        sends: 2,
        destroys_login: false,
        run: step_delete,
        what: "a text, then its delete for everyone",
    },
    StepDef {
        name: "modifiers-receive",
        sends: 0,
        destroys_login: false,
        run: step_modifiers_receive,
        what: "a reaction, a reply with a quote, an edit and a delete from the personal account",
    },
    StepDef {
        name: "offline-begin",
        sends: 0,
        destroys_login: false,
        run: step_offline_begin,
        what: "before the stop for the catch-up: remember the newest stored message. Opens every 1:1 and group chat, which marks them read in a focused window (hands off the window)",
    },
    StepDef {
        name: "offline-catch-up",
        sends: 0,
        destroys_login: false,
        run: step_offline_catch_up,
        what: "after the start: what was sent meanwhile arrived once, with its content (--expect <n>). Opens every 1:1 and group chat, which marks them read in a focused window (hands off the window)",
    },
    StepDef {
        name: "unlink-from-phone",
        sends: 0,
        destroys_login: true,
        run: step_unlink_from_phone,
        what: "OPT-IN, ENDS THE LOGIN: the phone unlinks the device; Kourier wipes its keys",
    },
];

fn list_steps() {
    println!("steps of the default run, in order (sends = the most messages a step sends):");
    for def in STEPS.iter().filter(|d| !d.destroys_login) {
        println!("  {:<20} sends {}  {}", def.name, def.sends, def.what);
    }
    println!("opt-in steps, each needs --destroys-login and is never part of the default run:");
    for def in STEPS.iter().filter(|d| d.destroys_login) {
        println!("  {:<20} sends {}  {}", def.name, def.sends, def.what);
    }
}

/// The app's log from byte `from` to byte `to`.
fn log_between(path: &Path, from: u64, to: u64) -> String {
    let mut bytes = Vec::new();
    if let Ok(mut f) = File::open(path)
        && f.seek(SeekFrom::Start(from)).is_ok()
    {
        let _ = f.take(to.saturating_sub(from)).read_to_end(&mut bytes);
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// The lines of a step's stretch of the app's log, shortened in the middle when long.
fn log_excerpt(path: &Path, from: u64, to: u64) -> Vec<String> {
    const HEAD: usize = 25;
    const TAIL: usize = 70;
    let text = log_between(path, from, to);
    let lines: Vec<String> = text.lines().map(redact).collect();
    if lines.len() <= HEAD + TAIL {
        return lines;
    }
    let mut out = lines[..HEAD].to_vec();
    out.push(format!(
        "… {} lines left out (bytes {from} to {to} of {}) …",
        lines.len() - HEAD - TAIL,
        path.display()
    ));
    out.extend_from_slice(&lines[lines.len() - TAIL..]);
    out
}

fn file_len(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

fn step(a: &Args) -> Result<i32> {
    let name = a.step.as_deref().unwrap_or_default();
    let def = STEPS
        .iter()
        .find(|d| d.name == name)
        .ok_or_else(|| anyhow!("unknown step {name:?} (see `cargo xtask smoke steps`)"))?;
    let dir = a.place.resolve(None, &vars)?;
    if def.destroys_login && !a.destroys_login {
        println!(
            "STEP {name} ENDS THE KEPT LOGIN of {}: {}.\nIt is not part of the default run. Nothing was done. To run it on purpose, with the user's say, add --destroys-login.",
            dir.dir.display(),
            def.what
        );
        return Ok(1);
    }
    let client = Client::connect(&dir)?;
    // Which profile and which server this acts on, first: a command without `--mock` means
    // the kept profile, and whoever reads the output sees it at once.
    println!(
        "STEP {name} on {} ({}): {}",
        dir.dir.display(),
        client.info()?["state"]["env"].as_str().unwrap_or("?"),
        def.what
    );
    let started = now_ms();
    let from = file_len(&dir.app_log());
    let mut run = StepRun {
        client: &client,
        dir: &dir,
        args: a,
        notes: Vec::new(),
        sends: 0,
        log_from: from,
    };
    let result = (def.run)(&mut run);
    // What the app logged for the last action is still on its way to the file.
    std::thread::sleep(Duration::from_millis(300));
    let (ended, to) = (now_ms(), file_len(&dir.app_log()));
    let (verdict, reason) = match &result {
        Ok(()) => ("PASS", String::new()),
        Err(e) => ("FAIL", redact(&format!("{e:#}"))),
    };
    for note in &run.notes {
        println!("NOTE: {note}");
    }
    println!(
        "{verdict} {name} ({}.{} s){}",
        (ended - started) / 1000,
        (ended - started) % 1000 / 100,
        if reason.is_empty() {
            String::new()
        } else {
            format!(": {reason}")
        }
    );
    println!("--- log of this step ({}) ---", dir.app_log().display());
    for line in log_excerpt(&dir.app_log(), from, to) {
        println!("{line}");
    }
    println!("--- end of the log of this step ---");
    append_line(
        &dir.steps_log(),
        &json!({
            "name": name,
            "result": verdict,
            "reason": reason,
            "started_ms": started,
            "ended_ms": ended,
            "sends": run.sends,
            "notes": run.notes,
            "log": {"from": from, "to": to},
        })
        .to_string(),
    )?;
    Ok(i32::from(result.is_err()))
}

fn step_link(run: &mut StepRun<'_>) -> Result<()> {
    let state = run.client.state()?;
    // A login that is already at its QR code (this step was interrupted while it waited) is
    // picked up where it stands: nothing is started twice.
    // ("waiting" is also what the login page says while no login runs.)
    let underway = matches!(state["login"].as_str(), Some("qr" | "progress"));
    let seq = if underway {
        run.note("a login was already underway: waiting for it, nothing was started again");
        let (events, _) = run.client.events(0)?;
        events
            .iter()
            .rev()
            .find(|e| e["event"] == "login_step" && e["data"]["kind"] == "input")
            .and_then(|e| e["seq"].as_u64())
            .unwrap_or(0)
    } else {
        if state["accounts"].as_array().is_some_and(|a| !a.is_empty()) {
            bail!(
                "this profile has an account already (accounts: {}). `link` is for a profile without one, and linking again is not part of the default run",
                state["accounts"]
            );
        }
        let (_, seq) = run.client.events(u64::MAX)?;
        run.client.ctl(
            "add_account",
            json!({"network": "Signal"}),
            Duration::from_secs(30),
        )?;
        run.client.wait("login:input", Duration::from_secs(30))?;
        let name = run.args.device_name.as_deref().unwrap_or(DEVICE_NAME);
        run.client.ctl(
            "submit_login",
            json!({"values": [name]}),
            Duration::from_secs(30),
        )?;
        // The first contact with the server: the provisioning socket.
        run.client
            .wait("login:qr", Duration::from_secs(90))
            .context("no QR code came up: the provisioning socket did not open (read the log)")?;
        seq
    };
    run.phone("on the TEST phone (never the personal one): Signal > Settings > Linked devices > Link a new device. Scan the QR code in the Kourier window and confirm. Keep the phone unlocked and online for a minute.");
    run.waiting("for the phone to scan a code (a new one every 45 s, six in all)");
    let shown = Instant::now();
    let outcome = poll(run.wait_secs(330), Duration::from_millis(500), || {
        let (events, _) = run.client.events(seq)?;
        for e in &events {
            let (name, data) = (e["event"].as_str().unwrap_or(""), &e["data"]);
            if name == "login_step" && data["kind"] == "done" {
                return Ok(Some(Ok(())));
            }
            if name == "error" && data["source"] == "login" {
                return Ok(Some(Err(data["message"]
                    .as_str()
                    .unwrap_or("failed")
                    .to_owned())));
            }
        }
        Ok(None)
    })?;
    match outcome {
        Some(Ok(())) => {}
        Some(Err(message)) if message.contains("not the expected one") => bail!(
            "the account guard refused the phone that scanned: it is not the account with the expected number. Nothing was linked ({message})"
        ),
        Some(Err(message)) => bail!(
            "the login failed: {message}. Look at the TEST phone's list of linked devices and remove a Kourier entry if one is there. 409: a capability is missing, stop. 411: too many devices. 499: the version is refused, stop"
        ),
        None => bail!(
            "no phone answered a QR code in time. Look at the TEST phone's list of linked devices before anything else. Do not stop the app while a link may be in flight"
        ),
    }
    run.note(format!(
        "linked {:.1} s after the first code was shown (contacts and Storage Service synced)",
        shown.elapsed().as_secs_f32()
    ));
    run.client
        .wait("account_active", Duration::from_secs(120))
        .context("linked, but the account did not come online (read the log for \"post-connect steps failed\")")?;
    let chats = parse_chats(
        &run.client
            .ctl("chats", json!({}), Duration::from_secs(30))?,
    );
    run.note(format!(
        "online; {} conversations listed ({} group)",
        chats.len(),
        chats.iter().filter(|c| c.kind == "group").count()
    ));
    run.note("on the TEST phone: Kourier is listed under Linked devices");
    Ok(())
}

fn step_passive(run: &mut StepRun<'_>) -> Result<()> {
    let secs = run.args.seconds.unwrap_or(300);
    let (_, seq) = run.client.events(u64::MAX)?;
    run.waiting(&format!("{secs} s, sending nothing"));
    let every = Duration::from_secs(if secs < 30 { 1 } else { 5 });
    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut samples = 0u32;
    while Instant::now() < deadline {
        let state = run.client.state()?;
        samples += 1;
        if state["accounts"] != json!(["online"]) {
            bail!(
                "the account left online after {samples} looks (accounts: {})",
                state["accounts"]
            );
        }
        std::thread::sleep(every);
    }
    let (events, _) = run.client.events(seq)?;
    let errors: Vec<String> = events
        .iter()
        .filter(|e| e["event"] == "error")
        .map(|e| e["data"].to_string())
        .collect();
    if !errors.is_empty() {
        bail!("the app reported errors meanwhile: {}", errors.join("; "));
    }
    let chats = parse_chats(
        &run.client
            .ctl("chats", json!({}), Duration::from_secs(30))?,
    );
    run.note(format!(
        "online at each of {samples} looks over {secs} s; {} direct and {} group chats; {} unread in all",
        chats.iter().filter(|c| c.kind == "direct").count(),
        chats.iter().filter(|c| c.kind == "group").count(),
        chats.iter().map(|c| c.unread).sum::<i64>()
    ));
    Ok(())
}

fn step_reconnect(run: &mut StepRun<'_>) -> Result<()> {
    let info = run.client.info()?;
    let state = State(info["state"].clone());
    if !state.linked() {
        bail!(
            "this profile is not linked: {}",
            login_lines(&state, now_ms()).join("; ")
        );
    }
    run.client
        .wait("account_active", Duration::from_secs(90))
        .context("the account did not come online. Do not link again blindly: read the log (401 or 403 at connect means the server dropped the device)")?;
    // Since this start of the app: no login step, no QR code.
    let (events, _) = run.client.events(0)?;
    let logins = events
        .iter()
        .filter(|e| matches!(e["event"].as_str(), Some("login_step" | "login_qr")))
        .count();
    if logins > 0 {
        bail!("the app showed {logins} login steps since it started: it did not simply reconnect");
    }
    for line in login_lines(&state, now_ms()) {
        run.note(line.lines().next().unwrap_or_default().to_owned());
    }
    run.note("online again without a QR code or a login step");
    run.note("on the TEST phone: the device shows as recently active");
    Ok(())
}

/// Waits for exactly one new message from others in one chat of `kind` and returns the chat.
fn one_arrives(run: &mut StepRun<'_>, kind: &str, instruction: &str) -> Result<i64> {
    let base = Snap::take(run.client)?;
    let base_newest = newest_message(&base.rows);
    run.phone(instruction);
    run.waiting("for the message");
    let seen = poll(run.wait_secs(300), Duration::from_secs(1), || {
        let now = Snap::take(run.client)?;
        let new = incoming(&base.chats, base.open, base_newest, &now, &[kind]);
        Ok((!new.is_empty()).then_some(()))
    })?;
    if seen.is_none() {
        bail!(
            "nothing arrived in a {kind} chat in time. Read the log for a decryption error or a waiting lane; an \"Undecryptable\" placeholder or a retry job is a finding to stop on"
        );
    }
    // A duplicate would follow at once: look again after a moment.
    std::thread::sleep(Duration::from_secs(3));
    let now = Snap::take(run.client)?;
    let new = incoming(&base.chats, base.open, base_newest, &now, &[kind]);
    match new.as_slice() {
        [(chat, 1)] => {
            run.note(format!(
                "one new message, once, in {kind} conversation #{chat}"
            ));
            Ok(*chat)
        }
        other => bail!(
            "expected one new message in one {kind} chat, found {other:?} (conversation, count)"
        ),
    }
}

fn remember(run: &mut StepRun<'_>, key: &str, chat: i64) -> Result<()> {
    let before = run.tool_state()?.0[key].as_i64();
    let mut set = json!({"op": "set", key: chat});
    if key == "peer_conv" && before != Some(chat) {
        // Another chat: the timer confirmed for the old one says nothing about this one.
        set["peer_timer_secs"] = Value::Null;
    }
    run.client.call(&set, Duration::from_secs(5))?;
    match before {
        Some(old) if old != chat => run.note(format!("{key} changed from #{old} to #{chat}")),
        _ => run.note(format!("conversation #{chat} is remembered as {key}")),
    }
    Ok(())
}

fn step_receive(run: &mut StepRun<'_>) -> Result<()> {
    let chat = one_arrives(
        run,
        "direct",
        "from the PERSONAL account, send one text message to the test account",
    )?;
    remember(run, "peer_conv", chat)?;
    run.note(
        "on the personal phone: the message shows as delivered; on the TEST phone: it arrived too",
    );
    Ok(())
}

fn step_group_receive(run: &mut StepRun<'_>) -> Result<()> {
    let chat = one_arrives(
        run,
        "group",
        "from the PERSONAL account, send one text message in the group both accounts are in",
    )?;
    remember(run, "group_conv", chat)?;
    Ok(())
}

fn step_read(run: &mut StepRun<'_>) -> Result<()> {
    let peer = run.remembered("peer_conv", "receive")?;
    // A settled look: the rows are there, and they say what `receive` could not see.
    let state = run.settled(peer)?;
    if placeholder_on_top(rows_in(&state)) {
        bail!(
            "the newest message from others in conversation #{peer} shows no content: it is a placeholder (a message that could not be decrypted, or a kind of message Kourier does not show), and not a message that arrived. `step receive` counts by the chat's unread counter, which such a row raises too. Read the app's log for a decryption error or a retry job"
        );
    }
    let is_read = |run: &StepRun<'_>| -> Result<Option<()>> {
        let chats = parse_chats(
            &run.client
                .ctl("chats", json!({}), Duration::from_secs(30))?,
        );
        Ok(chats
            .iter()
            .any(|c| c.id == peer && c.unread == 0)
            .then_some(()))
    };
    let mut read = poll(Duration::from_secs(15), Duration::from_millis(300), || {
        is_read(run)
    })?;
    if read.is_none() {
        // Kourier marks a chat read only while its window has the focus (as Signal Desktop
        // does): on a desktop the window may be behind another one.
        run.phone("on this computer: give the Kourier window the focus with a click on its title bar (or on an empty spot of the open chat's message area). Not on a chat in the list: that opens another chat");
        run.waiting("for the chat to be marked read");
        read = poll(run.wait_secs(120), Duration::from_millis(500), || {
            // A click on a chat in the list gave the window its focus and opened that chat:
            // the peer chat is opened again and is marked read now that the focus is there.
            if run.state_of(peer)?.is_none() {
                run.open(peer)?;
            }
            is_read(run)
        })?;
    }
    if read.is_none() {
        bail!(
            "conversation #{peer} still counts unread messages after it was opened: the Kourier window must have the focus for a chat to be marked read"
        );
    }
    run.note(format!(
        "conversation #{peer} was opened and counts no unread messages{}",
        if newest_from_others(rows_in(&state)).is_some_and(has_content) {
            "; its newest message from others shows its content"
        } else {
            ""
        }
    ));
    run.note(format!(
        "its disappearing-messages timer in Kourier: {}. Compare it with what the PERSONAL phone shows for this chat before `step send` (give that value as --timer)",
        chat_timer(&state).map_or_else(|| "unknown".to_owned(), timer_text)
    ));
    run.note("on the personal phone: the message shows as read, when both accounts have read receipts on; on the TEST phone: the chat is read");
    Ok(())
}

/// `--timer`: `off` or seconds, what the phone shows for the chat.
fn parse_timer(text: &str) -> Result<i64> {
    match text {
        "off" | "0" => Ok(0),
        _ => text
            .parse::<i64>()
            .ok()
            .filter(|s| *s > 0)
            .ok_or_else(|| anyhow!("--timer takes off or a number of seconds")),
    }
}

fn step_send(run: &mut StepRun<'_>) -> Result<()> {
    if run.args.timer.is_none() {
        bail!(
            "`step send` needs --timer <off|seconds>: the disappearing-messages timer the PERSONAL phone shows for this chat. Every 1:1 message restates the chat's timer, and a wrong value would change the setting on the personal account"
        );
    }
    let peer = run.peer_for_send()?;
    run.send_text("a text to the peer")?;
    run.note(format!("sent to conversation #{peer}"));
    // Delivery and read depend on the phones: watched for a while, reported, not required.
    let _ = run
        .client
        .wait("outgoing_state:Delivered", Duration::from_secs(20));
    let row = run.newest_own()?.unwrap_or_default();
    run.note(format!("its state after 20 s: {}", row["state"]));
    run.note("on the personal phone: the message arrived once, with no timer change and no system message; on the TEST phone: it shows as sent");
    Ok(())
}

fn step_group_send(run: &mut StepRun<'_>) -> Result<()> {
    let group = run.remembered("group_conv", "group-receive")?;
    run.open(group)?;
    run.send_text("a text to the group")?;
    run.note(format!("sent to group conversation #{group}"));
    // The app's line about the send says how it went out.
    std::thread::sleep(Duration::from_millis(400));
    let log = log_between(&run.dir.app_log(), run.log_from, u64::MAX);
    match sender_key_recipients(&log) {
        Some(0) => run.note("it went out pairwise: fewer than two other members qualify for a sender-key send, so the multi-recipient send was NOT exercised. That needs a group with at least two other members"),
        Some(n) => run.note(format!("it went out with a sender key to {n} members in one multi-recipient send; the log lines below name its route and status")),
        None => run.note("the log has no line about the group send"),
    }
    run.note("on the phones: the message arrived once on every device");
    Ok(())
}

/// How many members the newest group send in `log` reached with a sender key (the app's
/// "group message sent … sender_key=<n>" line).
fn sender_key_recipients(log: &str) -> Option<u64> {
    log.lines()
        .rev()
        .find(|l| l.contains("group message sent"))?
        .split("sender_key=")
        .nth(1)?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

/// The pictures of the open chat in messages from others that are newer than message
/// `after`: how many there are, and how many of them are downloaded and drawn.
///
/// dev-control lists the chat's media blocks apart from its rows (`state.media`): in the
/// order of the rows, one block per attachment of a row (`kinds`). The blocks of a row are
/// found by counting, and every block must fit its row (whose message, picture or file):
/// a state where they do not line up is an error and never a count.
///
/// A picture is told by its message and not by how many pictures the chat shows: that
/// number falls when a disappearing timer erases an older picture during the wait, and it
/// rises when the download of an older one ends.
fn new_pictures(state: &Value, after: i64) -> Result<(usize, usize)> {
    let mut blocks = state["media"].as_array().into_iter().flatten();
    let (mut arrived, mut shown) = (0, 0);
    for row in rows_in(state) {
        let new = row["own"] != json!(true) && row["message"].as_i64().is_some_and(|m| m > after);
        for kind in row["kinds"].as_array().into_iter().flatten() {
            let kind = kind.as_str().unwrap_or_default();
            let drawn = matches!(kind, "image" | "gif" | "video" | "sticker");
            let block = blocks
                .next()
                .filter(|b| b["own"] == row["own"] && (b["block"] == "image") == drawn)
                .ok_or_else(|| {
                    anyhow!("the app's state does not line up: the media blocks of the open chat are not the attachments of its rows, in their order. The tool cannot tell which message a picture belongs to")
                })?;
            if new && matches!(kind, "image" | "gif") {
                arrived += 1;
                if block["state"] == "local" && block["shown"] == json!(true) {
                    shown += 1;
                }
            }
        }
    }
    Ok((arrived, shown))
}

fn step_attachment_receive(run: &mut StepRun<'_>) -> Result<()> {
    let peer = run.remembered("peer_conv", "receive")?;
    // The base is a settled look: a state taken before the rows are loaded would make every
    // picture the chat already holds a new one.
    let base_newest = newest_message(&run.rows_of(peer)?);
    run.phone("from the PERSONAL account, send one photo to the test account");
    run.waiting("for the photo");
    let (mut asked, mut arrived) = (false, 0);
    let seen = poll(run.wait_secs(300), Duration::from_secs(1), || {
        let Some(state) = run.watched(peer, &mut asked)? else {
            return Ok(None);
        };
        let (new, shown) = new_pictures(&state, base_newest)?;
        arrived = new;
        Ok((shown > 0).then_some(()))
    })?;
    if seen.is_none() {
        let state = run.client.state()?;
        bail!(
            "{} (media blocks of the open chat: {}).{}",
            if arrived == 0 {
                "no new message with a picture arrived in time".to_owned()
            } else {
                format!(
                    "new pictures arrived ({arrived}), but none was downloaded and shown in time: read the log for the download, its CDN and its status"
                )
            },
            state["media"],
            run.elsewhere(peer)?
        );
    }
    run.note("a new message with a picture arrived; it was downloaded, decrypted and shown");
    Ok(())
}

fn step_attachment_send(run: &mut StepRun<'_>) -> Result<()> {
    run.peer_for_send()?;
    run.ready_to_send()?;
    let outbox = run.dir.smoke().join("outbox");
    private_dir(&outbox)?;
    let (picture, note) = (
        outbox.join("smoke-picture.png"),
        outbox.join("smoke-note.txt"),
    );
    std::fs::write(&picture, test_png(96, 64))?;
    std::fs::write(&note, "Kourier smoke test: a small text file.\n")?;
    run.client.ctl(
        "attach",
        json!({"paths": [picture.to_string_lossy(), note.to_string_lossy()]}),
        Duration::from_secs(30),
    )?;
    let planned = run.client.wait("files_planned", Duration::from_secs(60))?;
    let composer = &planned["observed"];
    let messages = composer["messages"].as_u64().unwrap_or(0);
    if composer["refused"].as_u64().unwrap_or(0) != 0 || !(1..=2).contains(&messages) {
        bail!("the composer did not plan the two files as expected ({composer}); nothing was sent");
    }
    run.sends += messages;
    run.client.ctl_sending(
        "send",
        json!({"text": ""}),
        messages,
        Duration::from_secs(120),
    )?;
    run.client
        .wait("none_pending", Duration::from_secs(180))
        .context(
            "the files were not sent in time (read the log for the upload's CDN number and status)",
        )?;
    let state = run.client.state()?;
    let own: Vec<&Value> = state["rows"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|r| r["own"] == json!(true))
        .take(usize::try_from(messages).unwrap_or(2))
        .collect();
    if !own.iter().all(|r| state_is_sent(r)) {
        bail!(
            "not every file message was sent: {:?}",
            own.iter().map(|r| &r["state"]).collect::<Vec<_>>()
        );
    }
    run.note(format!(
        "a picture and a text file went out as {messages} messages, all sent"
    ));
    run.note("on the personal phone: both open");
    Ok(())
}

fn step_react(run: &mut StepRun<'_>) -> Result<()> {
    run.peer_for_send()?;
    run.ready_to_send()?;
    run.sends += 1;
    run.client.ctl_sending(
        "act",
        json!({"on": "other", "action": "react", "emoji": "👍"}),
        1,
        Duration::from_secs(30),
    )?;
    let shown = poll(Duration::from_secs(30), Duration::from_millis(300), || {
        let state = run.client.state()?;
        let mine = state["rows"].as_array().into_iter().flatten().any(|r| {
            r["own"] != json!(true)
                && r["reactions"]
                    .as_array()
                    .is_some_and(|x| x.iter().any(|x| x["mine"] == json!(true)))
        });
        Ok(mine.then_some(()))
    })?;
    if shown.is_none() {
        bail!("the reaction does not show on the peer's message");
    }
    run.note("a thumbs-up on the peer's newest message; on the personal phone: it shows there");
    Ok(())
}

fn step_reply(run: &mut StepRun<'_>) -> Result<()> {
    run.peer_for_send()?;
    run.ready_to_send()?;
    run.sends += 1;
    run.client.ctl_sending(
        "act",
        json!({"on": "other", "action": "reply", "text": format!("Kourier smoke test: a reply with a quote, {}", utc(now_ms()))}),
        1,
        Duration::from_secs(30),
    )?;
    let quoted = poll(Duration::from_secs(60), Duration::from_millis(300), || {
        Ok(run
            .newest_own()?
            .filter(|r| r["quote"] == "known" && state_is_sent(r))
            .map(|_| ()))
    })?;
    if quoted.is_none() {
        bail!(
            "no sent reply with a quote shows (newest own row: {:?})",
            run.newest_own()?
        );
    }
    run.note("a reply that quotes the peer's newest message was sent; on the personal phone: the quote shows");
    Ok(())
}

fn step_edit(run: &mut StepRun<'_>) -> Result<()> {
    run.peer_for_send()?;
    run.send_text("a text that is edited next")?;
    run.ready_to_send()?;
    run.sends += 1;
    run.client.ctl_sending(
        "act",
        json!({"on": "own", "action": "edit", "text": format!("Kourier smoke test: the edited text, {}", utc(now_ms()))}),
        1,
        Duration::from_secs(30),
    )?;
    let edited = poll(Duration::from_secs(60), Duration::from_millis(300), || {
        Ok(run
            .newest_own()?
            .filter(|r| r["edited"] == json!(true) && state_is_sent(r))
            .map(|_| ()))
    })?;
    if edited.is_none() {
        bail!(
            "the edit does not show as sent (newest own row: {:?})",
            run.newest_own()?
        );
    }
    run.note("a text and its edit were sent; on the personal phone: the message shows the new text, marked as edited");
    Ok(())
}

fn step_delete(run: &mut StepRun<'_>) -> Result<()> {
    run.peer_for_send()?;
    run.send_text("a text that is deleted next")?;
    run.ready_to_send()?;
    let tombstones = |state: &Value| {
        state["rows"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|r| r["own"] == json!(true) && r["deleted"] == json!(true))
            .count()
    };
    let before = tombstones(&run.client.state()?);
    run.sends += 1;
    run.client.ctl_sending(
        "act",
        json!({"on": "own", "action": "delete", "for_everyone": true}),
        1,
        Duration::from_secs(30),
    )?;
    let gone = poll(Duration::from_secs(60), Duration::from_millis(300), || {
        Ok((tombstones(&run.client.state()?) > before).then_some(()))
    })?;
    if gone.is_none() {
        bail!("the deleted message does not show as deleted");
    }
    run.note("a text was sent and deleted for everyone; on the personal phone: it is gone");
    Ok(())
}

/// What the peer's modifiers leave in a chat, by kind: a reaction, a reply with a quote, an
/// edit, a delete. Each mark is named by its message, a reaction by its emoji too.
type Marks = [BTreeSet<(i64, String)>; 4];

/// The peer's marks in `rows` (dev-control `state.rows`): reactions of others (on any
/// message: a short disappearing timer may have erased ours, and a reaction on a message of
/// the peer's own proves the same path), their messages with a quote, their edited
/// messages, their deleted messages. Rows with an id below `from` are left out.
fn peer_marks(rows: &[Value], from: i64) -> Marks {
    let mut marks = Marks::default();
    for row in rows {
        let Some(id) = row["message"].as_i64().filter(|id| *id >= from) else {
            continue;
        };
        for chip in row["reactions"].as_array().into_iter().flatten() {
            // Someone else is behind a chip that counts more reactors than this account.
            if chip["count"].as_u64().unwrap_or(0) > u64::from(chip["mine"] == json!(true)) {
                marks[0].insert((id, chip["emoji"].as_str().unwrap_or_default().to_owned()));
            }
        }
        if row["own"] == json!(true) {
            continue;
        }
        for (kind, marked) in [
            (1, !row["quote"].is_null()),
            (2, row["edited"] == json!(true)),
            (3, row["deleted"] == json!(true)),
        ] {
            if marked {
                marks[kind].insert((id, String::new()));
            }
        }
    }
    marks
}

/// Which kinds of mark `now` holds that `base` lacks. Marks are compared by what they are,
/// not by how many there are: a reaction that replaces the peer's earlier one on the same
/// message (Signal keeps one per person and message) leaves the total as it was, and rows
/// that a disappearing timer erases during the wait lower it.
fn new_marks(base: &Marks, now: &Marks) -> [bool; 4] {
    std::array::from_fn(|kind| !now[kind].is_subset(&base[kind]))
}

fn step_modifiers_receive(run: &mut StepRun<'_>) -> Result<()> {
    const NAMES: [&str; 4] = ["a reaction", "a reply with a quote", "an edit", "a delete"];
    let peer = run.remembered("peer_conv", "receive")?;
    // The base is a settled look: a state taken before the rows are loaded would make every
    // mark the chat already holds a new one.
    let base_rows = run.rows_of(peer)?;
    // Rows older than the base look carry marks from before it. They come into view when
    // someone scrolls up, or opens the chat anew after a timer erased newer rows: no later
    // look counts them.
    let from = oldest_message(&base_rows);
    let base = peer_marks(&base_rows, from);
    run.phone("from the PERSONAL account, in the chat with the test account: react to one of the newest messages (when it already carries your reaction, with another emoji); send a reply that quotes a message; send a text and edit it; send a text and delete it for everyone");
    run.waiting("for all four");
    let (mut seen, mut asked) = ([false; 4], false);
    let all = poll(run.wait_secs(300), Duration::from_secs(1), || {
        let Some(state) = run.watched(peer, &mut asked)? else {
            return Ok(None);
        };
        let new = new_marks(&base, &peer_marks(rows_in(&state), from));
        for (was, is) in seen.iter_mut().zip(new) {
            *was |= is;
        }
        Ok(seen.iter().all(|s| *s).then_some(()))
    })?;
    let list = |want: bool| {
        (0..4)
            .filter(|i| seen[*i] == want)
            .map(|i| NAMES[i])
            .collect::<Vec<_>>()
            .join(", ")
    };
    if all.is_none() {
        bail!(
            "not all of them showed in time. Seen: {}. Missing: {}.{}",
            list(true),
            list(false),
            run.elsewhere(peer)?
        );
    }
    run.note(format!("shown in Kourier: {}", list(true)));
    Ok(())
}

const CAUGHT_UP_KINDS: [&str; 2] = ["direct", "group"];

/// What both offline steps ask of the user before they open every chat in turn.
const HANDS_OFF: &str = "on this computer: hands off the Kourier window until this step has ended. It opens every 1:1 and group chat in turn: a click on a chat fails the step, and what is typed meanwhile goes to whichever chat is open at that moment";

/// What opening every chat does, for the record of both offline steps.
const MARKS_CHATS_READ: &str = "this step opens every 1:1 and group chat in turn. In a focused Kourier window that marks each chat with unread messages read: read receipts go to their authors (when the account has read receipts on), a read sync goes to the test phone, and the disappearing timers of those messages start";

/// The format of the baseline file that `offline-begin` writes for `offline-catch-up`.
const BASELINE_VERSION: u64 = 2;

/// The baseline: the newest stored message id over the `chats` that were read.
fn baseline(newest: i64, chats: usize) -> Value {
    json!({"v": BASELINE_VERSION, "newest": newest, "chats": chats})
}

/// The newest stored message id of a baseline file. Only this version's format is taken: a
/// file without the version is refused. The first format (`{"chats": [...], "newest": N,
/// "open": M}`) kept under the same key the newest id of the chat that was open, and a
/// count from there would take older messages of the other chats for caught-up ones.
fn baseline_newest(base: &Value) -> Result<i64> {
    base["newest"]
        .as_i64()
        .filter(|_| base["v"].as_u64() == Some(BASELINE_VERSION))
        .ok_or_else(|| {
            anyhow!("the baseline is not one this version of the tool wrote: run `step offline-begin`, stop, send and start again")
        })
}

fn step_offline_begin(run: &mut StepRun<'_>) -> Result<()> {
    run.phone(HANDS_OFF);
    run.note(MARKS_CHATS_READ);
    let chats = run.rows_by_chat(&CAUGHT_UP_KINDS)?;
    if chats.is_empty() {
        // A baseline of no chat would make every stored message one that was caught up.
        bail!(
            "the list shows no 1:1 or group chat: there is nothing to remember. Is the app still loading its list? Nothing was written"
        );
    }
    let newest = chats
        .iter()
        .map(|(_, rows)| newest_message(rows))
        .max()
        .unwrap_or(0);
    write_private(
        &run.dir.baseline(),
        &baseline(newest, chats.len()).to_string(),
    )?;
    run.note(format!(
        "remembered the newest stored message of {} chats",
        chats.len()
    ));
    run.note("next, at once: `cargo xtask smoke stop`. Then the user sends the texts from the PERSONAL account (1:1 and group) and nothing else until the catch-up step has ended; `cargo xtask smoke start …`; `cargo xtask smoke step offline-catch-up --expect <n>`. Between the start and that step, opening chats in the window is fine; while the step runs, hands off the window again. Run that step soon after the start: a text that was read, in the window or on the test phone, is erased when its chat's disappearing timer ends, and the step cannot count it then");
    Ok(())
}

fn step_offline_catch_up(run: &mut StepRun<'_>) -> Result<()> {
    let expect = i64::try_from(run.args.expect.ok_or_else(|| {
        anyhow!("`step offline-catch-up` needs --expect <n>: how many messages were sent while the app was stopped")
    })?)?;
    let base = read_json(&run.dir.baseline())?
        .ok_or_else(|| anyhow!("no baseline: run `step offline-begin` before the stop"))?;
    let newest = baseline_newest(&base)?;
    run.phone(HANDS_OFF);
    run.note(MARKS_CHATS_READ);
    run.client
        .wait("account_active", Duration::from_secs(90))
        .context("the account did not come online")?;
    let count = |run: &StepRun<'_>| -> Result<Vec<Stored>> {
        Ok(stored_after(&run.rows_by_chat(&CAUGHT_UP_KINDS)?, newest))
    };
    run.waiting(&format!("for {expect} messages from the server's queue"));
    // A row without content ends the wait too: it is a finding, whatever follows it. A
    // count that may be short ends the step here, with the reason.
    let mut without_content = 0;
    poll(run.wait_secs(120), Duration::from_secs(1), || {
        let new = count(run)?;
        whole_count(&new)?;
        let empty: i64 = new.iter().map(|s| s.empty).sum();
        without_content = without_content.max(empty);
        let rows = empty + new.iter().map(|s| s.arrived).sum::<i64>();
        Ok((rows >= expect).then_some(()))
    })?;
    // More than expected would be a duplicate: look again after a moment.
    std::thread::sleep(Duration::from_secs(5));
    let by_chat = caught_up(&count(run)?, expect)?;
    let _ = std::fs::remove_file(run.dir.baseline());
    run.note(format!(
        "{expect} messages arrived once each after the start, each with its content: {by_chat:?} (conversation, count); their order is checked by eye in the window"
    ));
    if without_content > 0 {
        // The resend of a message that could not be decrypted fills its placeholder.
        run.note(format!(
            "an earlier look of this step showed new rows without content ({without_content}), and the last look shows none: most likely a message that could not be decrypted got its content with a second delivery. That is a finding although the count is right: read the app's log from the start on for the decryption error and the retry"
        ));
    }
    run.note("on the personal phone: they show as delivered after the catch-up");
    Ok(())
}

fn step_unlink_from_phone(run: &mut StepRun<'_>) -> Result<()> {
    let (_, seq) = run.client.events(u64::MAX)?;
    run.phone("on the TEST phone: Settings > Linked devices > the Kourier entry > Unlink");
    run.waiting("for the server to turn the device away");
    let out = poll(run.wait_secs(300), Duration::from_secs(1), || {
        let (events, _) = run.client.events(seq)?;
        Ok(events
            .iter()
            .any(|e| e["event"] == "account_state" && e["data"]["state"] == "logged_out")
            .then_some(()))
    })?;
    if out.is_none() {
        bail!(
            "the app did not report logged out in time (accounts: {})",
            run.client.state()?["accounts"]
        );
    }
    run.note("the app reports logged out and wiped the device's credentials and keys; the chats stay readable until the profile is removed");
    run.note("the log lines below show what the server answered (401, 403 or the 4401 close)");
    run.note("this profile's login has ended: `cargo xtask smoke wipe` removes it, or link it again by hand in the window");
    Ok(())
}

// ---- A small picture ----------------------------------------------------------------------------

/// A valid PNG: an RGB gradient, stored without compression.
fn test_png(width: u32, height: u32) -> Vec<u8> {
    let mut raw = Vec::new();
    for y in 0..height {
        raw.push(0);
        for x in 0..width {
            let (fx, fy) = (x * 255 / width.max(1), y * 255 / height.max(1));
            raw.extend_from_slice(&[
                u8::try_from(fx).unwrap_or(255),
                u8::try_from(fy).unwrap_or(255),
                u8::try_from(255 - fx / 2).unwrap_or(255),
            ]);
        }
    }
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut header = Vec::new();
    header.extend_from_slice(&width.to_be_bytes());
    header.extend_from_slice(&height.to_be_bytes());
    header.extend_from_slice(&[8, 2, 0, 0, 0]);
    png_chunk(&mut out, b"IHDR", &header);
    // zlib: stored blocks of at most 65535 bytes, then the Adler-32 of the data.
    let mut z = vec![0x78, 0x01];
    let mut blocks = raw.chunks(65_535).peekable();
    while let Some(block) = blocks.next() {
        let len = u16::try_from(block.len()).unwrap_or(u16::MAX);
        z.push(u8::from(blocks.peek().is_none()));
        z.extend_from_slice(&len.to_le_bytes());
        z.extend_from_slice(&(!len).to_le_bytes());
        z.extend_from_slice(block);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for byte in &raw {
        a = (a + u32::from(*byte)) % 65_521;
        b = (b + a) % 65_521;
    }
    z.extend_from_slice(&((b << 16) | a).to_be_bytes());
    png_chunk(&mut out, b"IDAT", &z);
    png_chunk(&mut out, b"IEND", &[]);
    out
}

fn png_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&u32::try_from(data.len()).unwrap_or(0).to_be_bytes());
    let start = out.len();
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let mut crc = 0xFFFF_FFFFu32;
    for byte in &out[start..] {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    out.extend_from_slice(&(!crc).to_be_bytes());
}

// ---- The rehearsal against the mock server ------------------------------------------------------

const MIN_NODE_MAJOR: u32 = 24;
const NVM_NODE: &str = ".nvm/versions/node/v24.21.0/bin/node";
/// Names and texts of the rehearsal's mock world. None of them may show up in anything the
/// tool prints or writes.
const PHONE_NAME: &str = "Rehearsal Testphone";
const PEER_NAME: &str = "Rehearsal Peerperson";
const GROUP_TITLE: &str = "Rehearsal Grouptitle";
const BYSTANDER_NAME: &str = "Rehearsal Bystander";
const PEER_TEXTS: [&str; 11] = [
    "peer text alpha for the receive step",
    "peer text bravo for the group",
    "peer text charlie with a quote",
    "peer text delta before its edit",
    "peer text echo after its edit",
    "peer text foxtrot to be deleted",
    "peer text golf while offline",
    "peer text hotel in the group while offline",
    "peer text india after the last restart",
    "peer text juliett in the group before the stop",
    "peer text kilo beside a row without content",
];

/// Node ≥ 24: `$KOURIER_NODE`, the nvm install, or `node` on `PATH`.
fn find_node() -> Result<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(n) = std::env::var_os("KOURIER_NODE").filter(|n| !n.is_empty()) {
        candidates.push(PathBuf::from(n));
    }
    if let Some(home) = std::env::var_os("HOME") {
        candidates.push(Path::new(&home).join(NVM_NODE));
    }
    candidates.push(PathBuf::from("node"));
    for node in candidates {
        let major = Command::new(&node)
            .arg("--version")
            .output()
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .trim()
                    .strip_prefix('v')?
                    .split('.')
                    .next()?
                    .parse::<u32>()
                    .ok()
            });
        if major.is_some_and(|m| m >= MIN_NODE_MAJOR) {
            return Ok(node);
        }
    }
    bail!("no Node ≥ {MIN_NODE_MAJOR} found; set KOURIER_NODE")
}

/// The mock harness (`tests/mock-harness`): JSON lines on stdin and stdout.
struct Harness {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: mpsc::Receiver<Value>,
    next_id: u64,
}

impl Harness {
    /// Starts the harness and returns it with its `ready` line (the handshake).
    fn start(node: &Path, state_dir: &Path, log: &Path) -> Result<(Self, Value)> {
        let mut child = Command::new(node)
            .arg("harness.mjs")
            .arg("--state-dir")
            .arg(state_dir)
            .current_dir(root().join("tests/mock-harness"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(File::create(log)?))
            .spawn()
            .context("start the mock harness")?;
        let stdout = child.stdout.take().context("harness stdout")?;
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if let Ok(v @ Value::Object(_)) = serde_json::from_str::<Value>(&line)
                    && tx.send(v).is_err()
                {
                    break;
                }
            }
        });
        let stdin = child.stdin.take();
        let harness = Self {
            child,
            stdin,
            lines,
            next_id: 0,
        };
        let ready = harness
            .lines
            .recv_timeout(Duration::from_secs(60))
            .context("the harness did not start")?;
        if ready["type"] != "ready" {
            bail!("the harness did not start: {ready}");
        }
        Ok((harness, ready))
    }

    fn request(&mut self, cmd: &str, args: Value, timeout: Duration) -> Result<Value> {
        self.next_id += 1;
        let id = self.next_id;
        let stdin = self.stdin.as_mut().context("harness stdin")?;
        writeln!(stdin, "{}", json!({"id": id, "cmd": cmd, "args": args}))?;
        stdin.flush()?;
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let v = self
                .lines
                .recv_timeout(left)
                .map_err(|_| anyhow!("harness: no answer to {cmd}"))?;
            if v["type"] == "fatal" {
                bail!("the harness crashed: {}", v["error"]);
            }
            if v["type"] == "response" && v["id"] == id {
                if v["ok"] == true {
                    return Ok(v["result"].clone());
                }
                bail!(
                    "harness {cmd} failed: {} ({})",
                    v["error"]["message"],
                    v["error"]["code"]
                );
            }
        }
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        drop(self.stdin.take());
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Runs this tool's own commands as separate processes, as the lead would.
struct Tool {
    exe: PathBuf,
    home: PathBuf,
    transcript: String,
}

impl Tool {
    /// `smoke <args> --home <home>`: its exit status and what it printed. `on_wait` runs once,
    /// when the command says it waits for the phone.
    fn run(
        &mut self,
        args: &[&str],
        mut on_wait: Option<&mut dyn FnMut() -> Result<()>>,
    ) -> Result<(i32, String)> {
        println!("\n$ cargo xtask smoke {}", args.join(" "));
        let mut child = Command::new(&self.exe)
            .arg("smoke")
            .args(args)
            .arg("--home")
            .arg(&self.home)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("run the smoke tool")?;
        let (tx, rx) = mpsc::channel::<String>();
        let out = child.stdout.take().context("stdout")?;
        let err = child.stderr.take().context("stderr")?;
        let tx_err = tx.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(out)
                .lines()
                .map_while(std::result::Result::ok)
            {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        std::thread::spawn(move || {
            for line in BufReader::new(err)
                .lines()
                .map_while(std::result::Result::ok)
            {
                if tx_err.send(line).is_err() {
                    break;
                }
            }
        });
        let deadline = Instant::now() + Duration::from_secs(900);
        let mut printed = String::new();
        loop {
            match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(line) => {
                    println!("  | {line}");
                    printed.push_str(&line);
                    printed.push('\n');
                    if line.starts_with("WAIT:")
                        && let Some(act) = on_wait.take()
                    {
                        act()?;
                    }
                }
                Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => {
                    let _ = child.kill();
                    bail!("`smoke {}` did not end in time", args.join(" "));
                }
            }
        }
        let status = child.wait()?;
        self.transcript.push_str(&printed);
        Ok((status.code().unwrap_or(-1), printed))
    }

    fn ok(&mut self, args: &[&str]) -> Result<String> {
        let (code, printed) = self.run(args, None)?;
        if code != 0 {
            bail!("`smoke {}` failed (exit {code})", args.join(" "));
        }
        Ok(printed)
    }

    /// The command must fail and say `needle`.
    fn fails(&mut self, args: &[&str], needle: &str) -> Result<String> {
        let (code, printed) = self.run(args, None)?;
        if code == 0 || !printed.contains(needle) {
            bail!(
                "`smoke {}` should have failed with {needle:?} (exit {code})",
                args.join(" ")
            );
        }
        Ok(printed)
    }

    /// A step that must pass; `on_wait` plays the phone.
    fn step(
        &mut self,
        args: &[&str],
        on_wait: Option<&mut dyn FnMut() -> Result<()>>,
    ) -> Result<String> {
        let mut all = vec!["step"];
        all.extend_from_slice(args);
        let (code, printed) = self.run(&all, on_wait)?;
        if code != 0 || !printed.contains(&format!("PASS {}", args[0])) {
            bail!("step {} did not pass (exit {code})", args[0]);
        }
        Ok(printed)
    }
}

fn base64(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for (i, shift) in [18, 12, 6, 0].into_iter().enumerate() {
            if i <= chunk.len() {
                out.push(char::from(ALPHABET[(n >> shift) as usize & 63]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn field(v: &Value, name: &str) -> Result<String> {
    v[name]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("harness result without {name:?}: {v}"))
}

/// `cargo xtask smoke rehearse`: the whole tool against the mock server, with the harness as
/// the phone: start, a refused second start, a link the account guard turns away, the link,
/// every step of the default run, the send cap, a stop and a start with a catch-up that is
/// counted after the chat was read, a second catch-up and a `read` that a row without
/// content fails, a start for another server that is refused without a wipe, the report.
/// Then everything the tool wrote is searched for the mock world's names, texts, ids,
/// numbers and the QR link.
fn rehearse(a: &Args) -> Result<i32> {
    let started = Instant::now();
    let node = find_node()?;
    if !root()
        .join("tests/mock-harness/node_modules/@signalapp/mock-server")
        .is_dir()
    {
        bail!("the mock harness is not installed: cd tests/mock-harness && npm ci (Node 24)");
    }
    if !a.no_build {
        run(
            "build kourier (debug, Signal backend)",
            cargo().args(["build", "-p", "kourier", "--features", "signal"]),
        )?;
    }
    let base = target_dir()
        .join("smoke-rehearsal")
        .join(format!("r{}", now_ms() / 1000));
    let profile = ProfileDir {
        dir: base.join("p"),
        keeper: false,
    };
    std::fs::create_dir_all(base.join("h"))?;
    let result = rehearsal(&node, &base, &profile);
    // Whatever happened, no app may stay behind on the throwaway profile.
    if Client::connect(&profile).is_ok() {
        let exe = std::env::current_exe()?;
        let _ = Command::new(exe)
            .args(["smoke", "stop", "--force", "--home"])
            .arg(&profile.dir)
            .status();
    }
    match result {
        Ok(steps) => {
            println!(
                "\nREHEARSAL PASS: {steps} steps passed against the mock server in {} s; nothing the tool wrote names a person, a number, a text or the QR link.",
                started.elapsed().as_secs()
            );
            if a.keep {
                println!("kept: {}", base.display());
            } else if base.starts_with(target_dir().join("smoke-rehearsal")) {
                let _ = std::fs::remove_dir_all(&base);
            }
            Ok(0)
        }
        Err(e) => {
            println!(
                "\nREHEARSAL FAIL: {e:#}\nkept for a look: {}",
                base.display()
            );
            Ok(1)
        }
    }
}

fn rehearsal(node: &Path, base: &Path, profile: &ProfileDir) -> Result<usize> {
    const T: Duration = Duration::from_secs(120);
    let (mut harness, ready) = Harness::start(node, &base.join("h"), &base.join("harness.log"))?;
    let handshake = base.join("hs.json");
    std::fs::write(&handshake, ready.to_string())?;
    let peer = harness.request("createPrimary", json!({"profileName": PEER_NAME}), T)?;
    let peer_ref = field(&peer, "ref")?;
    // A third member: with two others in the group, a group send goes out with a sender key
    // in one multi-recipient request.
    let bystander = harness.request("createPrimary", json!({"profileName": BYSTANDER_NAME}), T)?;
    let bystander_ref = field(&bystander, "ref")?;
    let phone = harness.request(
        "createPrimary",
        json!({"profileName": PHONE_NAME, "contacts": [peer_ref, bystander_ref]}),
        T,
    )?;
    let phone_ref = field(&phone, "ref")?;
    let group = harness.request(
        "createGroup",
        json!({"from": phone_ref, "title": GROUP_TITLE, "members": [peer_ref, bystander_ref]}),
        T,
    )?;
    let group_id = field(&group, "id")?;
    harness.request(
        "setStorageState",
        json!({
            "primary": phone_ref,
            "contacts": [
                {"ref": peer_ref, "whitelisted": true},
                {"ref": bystander_ref, "whitelisted": true},
            ],
            "groups": [{"id": group_id, "whitelisted": true}],
            "account": {"readReceipts": true, "typingIndicators": true},
        }),
        T,
    )?;
    let number = field(&phone, "e164")?;
    let tail: String = number
        .chars()
        .skip(number.chars().count().saturating_sub(4))
        .collect();
    let wrong_tail = format!(
        "{}{}",
        &tail[..3],
        (tail
            .chars()
            .last()
            .and_then(|c| c.to_digit(10))
            .unwrap_or(0)
            + 1)
            % 10
    );
    let env = format!("local:{}", handshake.display());
    let mut tool = Tool {
        exe: std::env::current_exe()?,
        home: profile.dir.clone(),
        transcript: String::new(),
    };
    let supervisor = Client {
        socket: profile.socket(),
    };
    let qr = || -> Result<String> {
        poll(Duration::from_secs(60), Duration::from_millis(200), || {
            let answer = supervisor.call(&json!({"op": "qr"}), Duration::from_secs(5))?;
            Ok(answer["url"].as_str().map(str::to_owned))
        })?
        .ok_or_else(|| anyhow!("the supervisor has no QR link"))
    };
    let devices = |harness: &mut Harness| -> Result<usize> {
        let list = harness.request("listDevices", json!({"primary": phone_ref}), T)?;
        Ok(list["devices"].as_array().map_or(0, Vec::len))
    };
    let mut passed = 0usize;

    // A start that expects another account, and a second start on the same profile.
    tool.ok(&[
        "start",
        "--env",
        &env,
        "--expect-tail",
        &wrong_tail,
        "--max-sends",
        "10",
        "--no-build",
    ])?;
    if lock_is_free(profile)? {
        bail!("the profile's lock is not held while the app runs");
    }
    tool.fails(&["start", "--env", &env, "--no-build"], "already running")?;
    // The wrong phone scans: the account guard turns it away, nothing is linked.
    let mut scan_in_vain = || -> Result<()> {
        let url = qr()?;
        let _ = harness.request(
            "completeProvision",
            json!({"url": url, "primary": phone_ref, "timeout": 8000}),
            Duration::from_secs(20),
        );
        Ok(())
    };
    let (code, printed) = tool.run(&["step", "link", "--wait", "60"], Some(&mut scan_in_vain))?;
    if code == 0 || !printed.contains("FAIL link") || !printed.contains("account guard") {
        bail!("a link by an unexpected account must fail at the account guard");
    }
    if devices(&mut harness)? != 1 {
        bail!("the refused link left a device on the account");
    }
    // The failed login left the app ready for the next one: the same step starts a new
    // login in the same app and gets to its QR code (nobody scans this one).
    let (code, printed) = tool.run(&["step", "link", "--wait", "1"], None)?;
    if code == 0
        || !printed.contains("no phone answered")
        || printed.contains("already underway")
        || printed.contains("has an account already")
    {
        bail!(
            "after a refused link, the next link step must start a new login and reach its QR code"
        );
    }
    tool.ok(&["stop"])?;

    // The expected account links.
    tool.ok(&["start", "--env", &env, "--expect-tail", &tail, "--no-build"])?;
    let mut device = String::new();
    let mut scan = || -> Result<()> {
        let url = qr()?;
        let linked = harness.request(
            "completeProvision",
            json!({"url": url, "primary": phone_ref, "timeout": 110_000}),
            T,
        )?;
        device = field(&linked, "ref")?;
        Ok(())
    };
    // A link step that gives up waiting (as a shell call that is cut off would) leaves the
    // login at its QR code, and the next one picks it up without starting another.
    let (code, printed) = tool.run(&["step", "link", "--wait", "1"], None)?;
    if code == 0 || !printed.contains("no phone answered") {
        bail!("a link step nobody scans for must fail on its wait");
    }
    if !tool
        .step(&["link"], Some(&mut scan))?
        .contains("a login was already underway")
    {
        bail!("the second link step started a login of its own");
    }
    passed += 1;
    harness.request(
        "waitForSync",
        json!({"primary": phone_ref, "device": device, "timeout": 60_000}),
        T,
    )?;
    let our_aci = device.split('.').next().unwrap_or_default().to_owned();
    let printed = tool.ok(&["state"])?;
    if !printed.contains("linked since") || !printed.contains("online") {
        bail!("`state` does not show the linked, online profile");
    }
    tool.step(&["passive", "--seconds", "5"], None)?;
    passed += 1;

    // 1:1: receive, read, send.
    let mut say = || -> Result<()> {
        harness.request(
            "sendText",
            json!({"from": peer_ref, "to": device, "text": PEER_TEXTS[0], "awaitDelivery": true, "timeout": 60_000}),
            T,
        )?;
        Ok(())
    };
    tool.step(&["receive", "--wait", "60"], Some(&mut say))?;
    tool.step(&["read"], None)?;
    passed += 2;
    // Opening the chat told the peer's phone that its message was read.
    let read_at = Instant::now();
    loop {
        let receipt = harness.request(
            "waitForReceipt",
            json!({"primary": peer_ref, "from": device, "timeout": 30_000}),
            T,
        )?;
        if receipt["type"] == "read" {
            break;
        }
        if read_at.elapsed() > Duration::from_secs(60) {
            bail!("the peer's phone got no read receipt after `step read`");
        }
    }
    // Before the chat's timer was compared with the phone's, no step sends to the peer.
    for step in ["react", "reply", "attachment-send", "edit", "delete"] {
        let (code, printed) = tool.run(&["step", step], None)?;
        if code == 0 || !printed.contains("was not compared") {
            bail!(
                "step {step} must not send before the chat's timer was compared with the phone's"
            );
        }
    }
    // A timer that is not the phone's: nothing goes out, on any step.
    for step in ["send", "reply"] {
        let (code, printed) = tool.run(&["step", step, "--timer", "3600"], None)?;
        if code == 0 || !printed.contains("nothing was sent") {
            bail!("step {step} with a timer that differs from the phone's must be refused");
        }
    }
    let used = |tool: &mut Tool| -> Result<bool> {
        Ok(tool.ok(&["state"])?.contains("sends:    0 of 10 used"))
    };
    if !used(&mut tool)? {
        bail!("a refused step counted or sent a message");
    }
    tool.step(&["send", "--timer", "off"], None)?;
    passed += 1;
    let got = harness.request(
        "waitForMessage",
        json!({"primary": peer_ref, "from": device, "kind": "message", "timeout": 30_000}),
        T,
    )?;
    if !got["text"]
        .as_str()
        .is_some_and(|t| t.starts_with("Kourier smoke test"))
    {
        bail!("the peer's phone did not get the tool's text");
    }
    let kourier_ts = got["timestamp"].clone();

    // The group.
    let mut group_say = || -> Result<()> {
        harness.request(
            "sendGroupText",
            json!({"from": peer_ref, "group": group_id, "to": [device], "text": PEER_TEXTS[1]}),
            T,
        )?;
        Ok(())
    };
    tool.step(&["group-receive", "--wait", "60"], Some(&mut group_say))?;
    if !tool
        .step(&["group-send"], None)?
        .contains("one multi-recipient send")
    {
        bail!("the group send to two other members did not go out with a sender key");
    }
    passed += 2;
    harness.request(
        "waitForMessage",
        json!({"primary": peer_ref, "from": device, "group": group_id, "kind": "message", "timeout": 30_000}),
        T,
    )?;

    // Attachments.
    let mut photo = || -> Result<()> {
        harness.request(
            "sendAttachments",
            json!({
                "from": peer_ref,
                "to": device,
                "attachments": [{"data": base64(&test_png(160, 120)), "contentType": "image/png", "width": 160, "height": 120}],
                "awaitDelivery": true,
                "timeout": 60_000,
            }),
            T,
        )?;
        Ok(())
    };
    tool.step(&["attachment-receive", "--wait", "90"], Some(&mut photo))?;
    tool.step(&["attachment-send"], None)?;
    passed += 2;
    for _ in 0..2 {
        let got = harness.request(
            "waitForMessage",
            json!({"primary": peer_ref, "from": device, "kind": "message", "timeout": 60_000}),
            T,
        )?;
        if !got["dataMessage"]["attachments"]
            .as_array()
            .is_some_and(|a| !a.is_empty())
        {
            bail!("the peer's phone got a file message without a file");
        }
    }

    // Modifiers from Kourier, each seen by the peer's phone.
    let next = |harness: &mut Harness, kind: &str| {
        harness.request(
            "waitForMessage",
            json!({"primary": peer_ref, "from": device, "kind": kind, "timeout": 60_000}),
            T,
        )
    };
    tool.step(&["react"], None)?;
    next(&mut harness, "reaction")?;
    tool.step(&["reply"], None)?;
    if next(&mut harness, "message")?["dataMessage"]["quote"].is_null() {
        bail!("the peer's phone got the reply without its quote");
    }
    tool.step(&["edit"], None)?;
    next(&mut harness, "message")?;
    harness.request(
        "waitForEdit",
        json!({"primary": peer_ref, "from": device, "timeout": 60_000}),
        T,
    )?;
    tool.step(&["delete"], None)?;
    next(&mut harness, "message")?;
    next(&mut harness, "delete")?;
    passed += 4;
    // Ten messages were sent, and ten is this run's cap: one more is refused.
    let (code, printed) = tool.run(&["step", "send", "--timer", "off"], None)?;
    if code == 0 || !printed.contains("send cap") {
        bail!("a send beyond the cap must be refused");
    }

    // Modifiers from the peer.
    let peer_aci = field(&peer, "aci")?;
    let mut modify = || -> Result<()> {
        let mut send = |args: Value| -> Result<Value> {
            let mut args = args;
            args["from"] = json!(peer_ref);
            args["to"] = json!(device);
            args["awaitDelivery"] = json!(true);
            args["timeout"] = json!(60_000);
            harness.request("sendData", args, T)
        };
        let quoting = send(
            json!({"text": PEER_TEXTS[2], "quote": {"id": kourier_ts, "author": our_aci, "text": "quoted"}}),
        )?;
        // The reaction is on a message of the peer's own: whose message carries it must not
        // matter (a rule that takes reactions on Kourier's messages only fails here).
        send(
            json!({"reaction": {"emoji": "🎉", "targetAuthor": peer_aci, "targetTimestamp": quoting["timestamp"]}}),
        )?;
        let original = send(json!({"text": PEER_TEXTS[3]}))?;
        send(json!({"text": PEER_TEXTS[4], "edit": {"targetTimestamp": original["timestamp"]}}))?;
        let doomed = send(json!({"text": PEER_TEXTS[5]}))?;
        send(json!({"delete": {"targetTimestamp": doomed["timestamp"]}}))?;
        Ok(())
    };
    tool.step(&["modifiers-receive", "--wait", "120"], Some(&mut modify))?;
    passed += 1;

    // Stop, messages while offline, start: the catch-up, without a QR code.
    // First one more text in the group, so that the newest stored message is not in the chat
    // that is open (the peer chat): a baseline taken from the open chat alone would count
    // this text as caught up.
    let group_unread = || -> Result<i64> {
        let chats = parse_chats(&supervisor.ctl("chats", json!({}), Duration::from_secs(30))?);
        Ok(chats
            .iter()
            .filter(|c| c.kind == "group")
            .map(|c| c.unread)
            .sum())
    };
    let unread_before = group_unread()?;
    harness.request(
        "sendText",
        json!({"from": peer_ref, "to": device, "group": group_id, "text": PEER_TEXTS[9], "awaitDelivery": true, "timeout": 60_000}),
        T,
    )?;
    // (Delivered is stored by the backend; the list shows it a moment later.)
    if poll(Duration::from_secs(30), Duration::from_millis(200), || {
        Ok((group_unread()? > unread_before).then_some(()))
    })?
    .is_none()
    {
        bail!("the group text sent before `offline-begin` did not show in the list");
    }
    tool.step(&["offline-begin"], None)?;
    tool.ok(&["stop"])?;
    if !lock_is_free(profile)? {
        bail!("the profile's lock is still held after the stop");
    }
    // (Signal names a message by its author and its sent time: the three get times of
    // their own, which two requests in the same millisecond would not have.)
    let sent_at = now_ms();
    for (i, text) in [PEER_TEXTS[6], PEER_TEXTS[0]].into_iter().enumerate() {
        harness.request(
            "sendText",
            json!({"from": peer_ref, "to": device, "text": text, "timestamp": sent_at + 10 * i as u64}),
            T,
        )?;
    }
    harness.request(
        "sendGroupText",
        json!({"from": peer_ref, "group": group_id, "to": [device], "text": PEER_TEXTS[7], "timestamp": sent_at + 50}),
        T,
    )?;
    let printed = tool.ok(&["start", "--env", &env, "--no-build"])?;
    if !printed.contains("reconnected without a QR code") || !printed.contains("linked since") {
        bail!("a start on the linked profile must reconnect and say since when it is linked");
    }
    tool.step(&["reconnect"], None)?;
    // The peer chat is opened and read before the count, as a user at the window would:
    // its two texts are not unread any more, and a count by unread growth would find one.
    tool.step(&["read"], None)?;
    tool.step(&["offline-catch-up", "--expect", "3", "--wait", "90"], None)?;
    passed += 4;
    // A second round for a row without content: one text and a message of a kind Kourier
    // does not show (a placeholder row, as one that could not be decrypted) go out while the
    // app is stopped. The placeholder comes last: it is the peer chat's newest row then.
    // (The stop and the start are the ones the next block needs anyway.)
    tool.step(&["offline-begin"], None)?;
    passed += 1;
    tool.ok(&["stop"])?;
    let sent_at = now_ms();
    harness.request(
        "sendText",
        json!({"from": peer_ref, "to": device, "text": PEER_TEXTS[10], "timestamp": sent_at}),
        T,
    )?;
    harness.request(
        "sendData",
        json!({"from": peer_ref, "to": device, "unsupported": "contact", "timestamp": sent_at + 10}),
        T,
    )?;

    // A start for another server: refused by the tool, and the login is still there.
    let mut elsewhere = ready.clone();
    elsewhere["port"] = json!(ready["port"].as_u64().unwrap_or(1) + 1);
    let other = base.join("hs-other.json");
    std::fs::write(&other, elsewhere.to_string())?;
    tool.fails(
        &[
            "start",
            "--env",
            &format!("local:{}", other.display()),
            "--no-build",
        ],
        "belongs to",
    )?;
    tool.fails(&["step", "unlink-from-phone"], "ENDS THE KEPT LOGIN")?;
    tool.fails(&["wipe"], "Nothing was deleted")?;
    tool.ok(&["start", "--env", &env, "--no-build"])?;
    tool.step(&["reconnect"], None)?;
    passed += 1;
    if devices(&mut harness)? != 2 {
        bail!("the device is not on the account any more");
    }
    // The placeholder is not an arrived message: the step fails and says why, although the
    // one text makes the expected count (a count of all rows would find two and say that).
    let (code, printed) = tool.run(
        &["step", "offline-catch-up", "--expect", "1", "--wait", "60"],
        None,
    )?;
    if code == 0 || !printed.contains("show no content") {
        bail!("a new row without content must fail the catch-up as a message that did not arrive");
    }
    // `receive` counts by unread counters and takes such a row for a message: `step read`,
    // which opens the peer chat after it, fails on a placeholder as the chat's newest row.
    let (code, printed) = tool.run(&["step", "read"], None)?;
    if code == 0 || !printed.contains("shows no content") {
        bail!("`step read` must fail on a placeholder as the newest message of the peer chat");
    }
    // The kept login works: a message sent now arrives on the reconnected device.
    let mut say_again = || -> Result<()> {
        harness.request(
            "sendText",
            json!({"from": peer_ref, "to": device, "text": PEER_TEXTS[8], "awaitDelivery": true, "timeout": 60_000}),
            T,
        )?;
        Ok(())
    };
    tool.step(&["receive", "--wait", "60"], Some(&mut say_again))?;
    passed += 1;
    // The peer chat is the same one: the timer confirmed for it still stands.
    if !tool.ok(&["state"])?.contains("timer confirmed off") {
        bail!("the confirmed timer of the peer chat was lost");
    }
    if !tool.ok(&["report"])?.contains("report:") {
        bail!("`report` did not say where it wrote");
    }
    tool.ok(&["stop"])?;
    drop(harness);

    // The launch order of a start is used up: the supervisor run by hand starts nothing.
    if profile.launch_file().exists() {
        bail!("the launch order was left behind after the start");
    }
    let by_hand = Command::new(std::env::current_exe()?)
        .args(["smoke", "__supervise", "--home"])
        .arg(&profile.dir)
        .stdin(Stdio::null())
        .output()
        .context("run the supervisor by hand")?;
    if by_hand.status.success()
        || !String::from_utf8_lossy(&by_hand.stderr).contains("no launch order")
        || !lock_is_free(profile)?
    {
        bail!("the supervisor started by hand must refuse to start the app");
    }

    // What the tool left behind.
    let mode = |path: &Path| std::fs::metadata(path).map(|m| m.permissions().mode() & 0o777);
    if mode(&profile.dir)? != 0o700 || mode(&profile.smoke())? != 0o700 {
        bail!("the profile directory is not private (0700)");
    }
    for file in [
        profile.state_file(),
        profile.app_log(),
        profile.events_log(),
    ] {
        if mode(&file)? != 0o600 {
            bail!("{} is not private (0600)", file.display());
        }
    }
    let state = State::load(profile)?.context("state file")?;
    if state.num("sends_used") != Some(10) || !state.linked() {
        bail!("the state file is off: {}", state.0);
    }
    let report = std::fs::read_to_string(profile.report())?;
    if report.matches("| PASS |").count() < passed {
        bail!("the report lists fewer passed steps than ran");
    }
    if walk_files(&profile.dir.join("kourier/run/media")) != 0 {
        bail!("plaintext media copies are left in the profile's run directory");
    }
    // Nothing the lead reads names a person, a number, a text, an id or the link.
    let mut readable = vec![("the tool's output".to_owned(), tool.transcript.clone())];
    let mut files = vec![
        profile.app_log(),
        profile.events_log(),
        profile.steps_log(),
        profile.report(),
        profile.supervisor_log(),
        profile.state_file(),
    ];
    if let Ok(logs) = std::fs::read_dir(profile.dir.join("kourier/state/logs")) {
        files.extend(logs.flatten().map(|e| e.path()));
    }
    for file in files {
        readable.push((
            file.display().to_string(),
            String::from_utf8_lossy(&std::fs::read(&file).unwrap_or_default()).into_owned(),
        ));
    }
    let mut forbidden: Vec<String> = vec![
        "sgnl://linkdevice".to_owned(),
        "pub_key=".to_owned(),
        PHONE_NAME.to_owned(),
        PEER_NAME.to_owned(),
        BYSTANDER_NAME.to_owned(),
        GROUP_TITLE.to_owned(),
        number,
        field(&peer, "e164")?,
        field(&peer, "aci")?,
        field(&phone, "aci")?,
        field(&bystander, "e164")?,
        field(&bystander, "aci")?,
        // The device's name and the texts the tool itself sends.
        DEVICE_NAME.to_owned(),
    ];
    forbidden.extend(PEER_TEXTS.iter().map(|t| (*t).to_owned()));
    for (name, text) in &readable {
        if text.is_empty() && name.ends_with("app.log") {
            bail!("{name} is empty: the app's log was not captured");
        }
        for needle in &forbidden {
            if text.contains(needle.as_str()) {
                bail!(
                    "{name} contains something it must never hold (one of the mock world's names, numbers, ids, texts or the link)"
                );
            }
        }
    }
    let app_log = &readable[1].1;
    for needed in ["kourier_signal::requests", "route=", "status=", "ms="] {
        if !app_log.contains(needed) {
            bail!("the app's log does not show requests with route, status and timing ({needed})");
        }
    }
    Ok(passed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Result<Args> {
        parse_args(&a.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>())
    }

    /// A scratch directory below the target directory, removed when dropped.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = target_dir()
                .join("tmp")
                .join(format!("smoke-test-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("scratch");
            Self(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn home_vars(key: &str) -> Option<String> {
        match key {
            "HOME" => Some("/home/u".to_owned()),
            _ => None,
        }
    }

    #[test]
    fn arguments() {
        let a = args(&[
            "start",
            "--env",
            "production",
            "--confirmed-by-user",
            "--expect-tail",
            "1234",
            "--profile",
            "second",
            "--max-sends",
            "12",
            "--offscreen",
        ])
        .expect("parses");
        assert_eq!(a.command, "start");
        assert_eq!(a.env, Some(EnvArg::Production));
        assert!(a.confirmed);
        assert_eq!(a.expect_tail.as_deref(), Some("1234"));
        assert_eq!(a.place.profile.as_deref(), Some("second"));
        assert_eq!((a.max_sends, a.offscreen), (Some(12), Some(true)));
        let a = args(&["step", "send", "--timer", "off", "--wait", "30"]).expect("parses");
        assert_eq!(a.step.as_deref(), Some("send"));
        assert_eq!((a.timer.as_deref(), a.wait), (Some("off"), Some(30)));
        assert!(!a.destroys_login && !a.yes_delete && !a.confirmed);
        for bad in [
            &["step"][..],
            &["step", "--wait"],
            &["start", "--env"],
            &["start", "--env", "prod"],
            &["start", "--max-sends", "many"],
            &["stop", "--now"],
        ] {
            assert!(args(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn environments() {
        assert_eq!(EnvArg::parse("production").expect("p"), EnvArg::Production);
        assert_eq!(EnvArg::parse("staging").expect("s"), EnvArg::Staging);
        assert!(EnvArg::parse("local:").is_err());
        assert!(EnvArg::parse("").is_err());
        assert_eq!(EnvArg::Production.binding().expect("p"), "production");
        assert_eq!(EnvArg::Staging.for_app(), "staging");
        let scratch = Scratch::new("env");
        let hs = scratch.0.join("hs.json");
        std::fs::write(&hs, r#"{"type":"ready","host":"localhost","port":4433}"#).expect("hs");
        let local = EnvArg::parse(&format!("local:{}", hs.display())).expect("local");
        assert!(local.is_local() && !EnvArg::Production.is_local());
        // The same name the app stores at link (`account.env`).
        assert_eq!(local.binding().expect("binding"), "local:localhost:4433");
        assert_eq!(local.for_app(), format!("local:{}", hs.display()));
        std::fs::write(&hs, "{}").expect("hs");
        assert!(local.binding().is_err());
        assert!(
            EnvArg::Local(scratch.0.join("missing.json"))
                .binding()
                .is_err()
        );
    }

    /// Without the user's go nothing is started and nothing is created, whatever else is
    /// on the command line.
    #[test]
    fn a_real_server_needs_the_users_go() {
        // A home that does not exist and cannot be made: the test reads and writes nothing of
        // the user's, and a start that got too far would fail on it.
        let nowhere =
            |key: &str| (key == "HOME").then(|| "/nonexistent-smoke-test-home".to_owned());
        let made = keeper_root(&nowhere).expect("root");
        for env in ["production", "staging"] {
            let a = args(&["start", "--env", env, "--expect-tail", "1234", "--no-build"])
                .expect("parses");
            let error = start_in(&a, &nowhere).expect_err("refused").to_string();
            assert!(error.contains("--confirmed-by-user"), "{error}");
            assert!(error.contains("Nothing was started"), "{error}");
        }
        // With the go but without the expected account, a first link is refused too, again
        // before anything is made or built (on a machine without disk encryption the start
        // stops one check earlier).
        let a = args(&[
            "start",
            "--env",
            "production",
            "--confirmed-by-user",
            "--no-build",
        ])
        .expect("parses");
        let error = start_in(&a, &nowhere).expect_err("refused").to_string();
        assert!(
            error.contains("--expect-tail") || error.contains("disk encryption"),
            "{error}"
        );
        assert!(!made.exists(), "{} was created", made.display());
    }

    /// Nothing the shell has set for a debug build reaches the app.
    #[test]
    fn inherited_kourier_variables_are_stripped() {
        let keys = [
            "KOURIER_SIGNAL_ENV",
            "KOURIER_FAILPOINT",
            "KOURIER_HOME",
            "KOURIER_LOG",
            "KOURIER_DEV_DEMO_STATE",
            "PATH",
            "WAYLAND_DISPLAY",
            "MY_KOURIER_NOTE",
        ];
        let stripped = inherited_to_strip(keys.iter().map(std::ffi::OsString::from));
        assert_eq!(
            stripped,
            keys[..5]
                .iter()
                .map(std::ffi::OsString::from)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn profiles_resolve_to_their_places() {
        let place = |a: &[&str]| args(a).expect("parses").place;
        let keeper = |name: &str| PathBuf::from("/home/u/.local/share/kourier-smoke").join(name);
        // A real server: the kept profile, "default" unless named.
        let dir = place(&["state"])
            .resolve(Some(&EnvArg::Production), &home_vars)
            .expect("default");
        assert_eq!((dir.dir, dir.keeper), (keeper("default"), true));
        let dir = place(&["state", "--profile", "two"])
            .resolve(None, &home_vars)
            .expect("named");
        assert_eq!((dir.dir, dir.keeper), (keeper("two"), true));
        let xdg = |k: &str| (k == "XDG_DATA_HOME").then(|| "/data".to_owned());
        assert_eq!(
            place(&["state"]).resolve(None, &xdg).expect("xdg").dir,
            PathBuf::from("/data/kourier-smoke/default")
        );
        // The mock: a throwaway directory below the target directory, never a keeper.
        let local = EnvArg::Local(PathBuf::from("/x/hs.json"));
        let dir = place(&["start"])
            .resolve(Some(&local), &home_vars)
            .expect("mock default");
        assert_eq!(dir.dir, mock_dir());
        assert!(!dir.keeper && dir.dir.starts_with(target_dir()));
        assert_eq!(
            place(&["state", "--mock"])
                .resolve(None, &home_vars)
                .expect("mock")
                .dir,
            mock_dir()
        );
        // A mock run never takes a kept profile, and --mock never means a real server.
        assert!(
            place(&["start", "--profile", "m"])
                .resolve(Some(&local), &home_vars)
                .is_err()
        );
        assert!(
            !place(&["start", "--home", "/somewhere/else"])
                .resolve(Some(&local), &home_vars)
                .expect("home")
                .keeper
        );
        assert!(
            place(&["start", "--mock"])
                .resolve(Some(&EnvArg::Production), &home_vars)
                .is_err()
        );
        let dir = place(&["state", "--home", "/somewhere/else"])
            .resolve(None, &home_vars)
            .expect("home");
        assert_eq!(
            (dir.dir, dir.keeper),
            (PathBuf::from("/somewhere/else"), false)
        );
        for bad in [
            &["state", "--profile", "a/b"][..],
            &["state", "--profile", ""],
            &["state", "--profile", "x", "--mock"],
        ] {
            assert!(place(bad).resolve(None, &home_vars).is_err(), "{bad:?}");
        }
        assert!(
            place(&["state"]).resolve(None, &|_| None).is_err(),
            "no HOME"
        );
    }

    #[test]
    fn a_login_is_not_kept_where_a_clean_up_reaches() {
        for dir in [
            root().join("dev-data"),
            target_dir().join("smoke-mock/default"),
        ] {
            assert!(
                unsafe_place_for_a_login(&dir, &home_vars).is_some(),
                "{}",
                dir.display()
            );
        }
        for base in ["/home/u", "/home/u/.local/share", "/home/u/.config"] {
            assert!(
                unsafe_place_for_a_login(Path::new(base), &home_vars).is_some(),
                "{base}"
            );
        }
        assert_eq!(
            unsafe_place_for_a_login(
                Path::new("/home/u/.local/share/kourier-smoke/default"),
                &home_vars
            ),
            None
        );
        // A directory that already holds a normal profile's data.
        let scratch = Scratch::new("normal");
        std::fs::create_dir_all(scratch.0.join("kourier/accounts")).expect("dirs");
        assert!(
            unsafe_place_for_a_login(&scratch.0, &|_| None)
                .is_some_and(|why| why.contains("normal Kourier profile") || why.contains("target"))
        );
    }

    #[test]
    fn the_idle_deadline() {
        const DAY: u64 = 86_400_000;
        let now = 100 * DAY;
        assert_eq!(idle(now, now), Idle::Fine(0));
        assert_eq!(idle(now - 20 * DAY, now), Idle::Fine(20));
        assert_eq!(idle(now - 21 * DAY, now), Idle::Warn(21));
        assert_eq!(idle(now - 29 * DAY, now), Idle::Warn(29));
        assert_eq!(idle(now - 30 * DAY, now), Idle::Overdue(30));
        // A clock set back does not count as time without a connection.
        assert_eq!(idle(now + DAY, now), Idle::Fine(0));

        let mut state = State::new("production", "production", 30, 0);
        assert_eq!(login_lines(&state, now), ["not linked yet"]);
        assert!(!state.linked());
        state.set("linked_since_ms", json!(10 * DAY));
        state.set("last_online_ms", json!(now - 2 * DAY));
        assert!(state.linked());
        let lines = login_lines(&state, now);
        assert_eq!(lines[0], "linked since 1970-01-11 00:00:00 UTC");
        assert!(
            lines[1].starts_with("last connected 1970-04-09 00:00:00 UTC (2 days ago)"),
            "{lines:?}"
        );
        assert!(!lines[1].contains("WARNING"));
        state.set("last_online_ms", json!(now - 25 * DAY));
        let lines = login_lines(&state, now);
        assert!(
            lines[1].contains("WARNING") && lines[1].contains("5 days are left"),
            "{lines:?}"
        );
        state.set("last_online_ms", json!(now - 40 * DAY));
        assert!(login_lines(&state, now)[1].contains("probably removed this device"));
        state.set("logged_out_ms", json!(now));
        assert!(!state.linked());
        assert!(login_lines(&state, now)[0].starts_with("the login ended on"));
    }

    #[test]
    fn redaction() {
        assert_eq!(
            redact("no session with 9d0652a3-dcc3-4d11-975f-74d61598733f.2 for +14155550101"),
            "no session with …733f.2 for +[number]"
        );
        assert_eq!(
            redact("qml: sgnl://linkdevice?uuid=a&pub_key=b shown"),
            "qml: sgnl://[redacted] shown"
        );
        assert_eq!(
            redact("signalcaptcha://x.y.z"),
            "signalcaptcha://[redacted]"
        );
        for same in [
            "sgnl:// link: showing the window",
            "account=aci:…1f2e route=/v2/keys status=200 ms=41",
            "size +123456, 9d0652a3-dcc3-4d11 and a+14155550101",
            "größe … ✓",
            "",
        ] {
            assert_eq!(redact(same), same);
        }
    }

    #[test]
    fn dates() {
        assert_eq!(utc(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(utc(1_759_845_296_000), "2025-10-07 13:54:56 UTC");
        assert_eq!(utc(951_782_400_000), "2000-02-29 00:00:00 UTC");
        assert_eq!(utc_compact(1_759_845_296_000), "20251007-135456");
    }

    fn chat(id: i64, kind: &str, unread: i64) -> Chat {
        Chat {
            id,
            kind: kind.to_owned(),
            is_self: false,
            unread,
        }
    }

    /// New messages are counted by unread growth, and in the open chat (read at once) by
    /// its new rows from others.
    #[test]
    fn incoming_messages_are_counted_per_chat() {
        let base = vec![
            chat(1, "direct", 0),
            chat(2, "group", 3),
            chat(3, "direct", 1),
        ];
        let mut now = Snap {
            chats: vec![
                chat(1, "direct", 1),
                chat(2, "group", 5),
                chat(3, "direct", 1),
                chat(4, "direct", 2),
            ],
            open: 0,
            rows: Vec::new(),
        };
        assert_eq!(incoming(&base, 0, 0, &now, &["direct"]), [(1, 1), (4, 2)]);
        assert_eq!(incoming(&base, 0, 0, &now, &["group"]), [(2, 2)]);
        assert_eq!(incoming(&base, 0, 0, &now, &["direct", "group"]).len(), 3);
        // The note to self never counts.
        now.chats[0].is_self = true;
        assert_eq!(incoming(&base, 0, 0, &now, &["direct"]), [(4, 2)]);
        // Chat 3 is open before and after: its unread count stays, its rows tell.
        let open = Snap {
            chats: vec![chat(3, "direct", 0)],
            open: 3,
            rows: vec![
                json!({"message": 12, "own": false}),
                json!({"message": 11, "own": true}),
                json!({"message": 10, "own": false}),
                json!({"message": 9, "own": false}),
            ],
        };
        assert_eq!(incoming(&base, 3, 9, &open, &["direct"]), [(3, 2)]);
        assert_eq!(incoming(&base, 3, 12, &open, &["direct"]), []);
        assert_eq!(newest_message(&open.rows), 12);
    }

    /// A text row as dev-control lists it.
    fn text_row(message: i64, own: bool) -> Value {
        json!({"message": message, "own": own, "deleted": false, "text": true, "attachments": 0})
    }

    fn stored(chat: i64, arrived: i64, empty: i64) -> Stored {
        Stored {
            chat,
            arrived,
            empty,
            cut: false,
        }
    }

    /// The offline catch-up counts stored rows from others, per chat, whatever was read
    /// since (unread counters fall when a chat is opened in the window).
    #[test]
    fn caught_up_messages_are_counted_by_stored_rows() {
        let chats = vec![
            (
                2,
                vec![
                    text_row(29, false),
                    text_row(28, true),
                    text_row(27, false),
                    text_row(25, false),
                ],
            ),
            (1, vec![text_row(26, false), text_row(20, false)]),
            (3, vec![text_row(4, false)]),
        ];
        assert_eq!(stored_after(&chats, 25), [stored(2, 2, 0), stored(1, 1, 0)]);
        assert_eq!(stored_after(&chats, 29), []);
        let all = stored_after(&chats, 0);
        assert_eq!(all, [stored(2, 3, 0), stored(1, 2, 0), stored(3, 1, 0)]);
        assert_eq!(per_chat(&all, |s| s.arrived), [(2, 3), (1, 2), (3, 1)]);
        assert_eq!(per_chat(&all, |s| s.empty), []);
        // A row without a message id (none today) never counts.
        let nameless = json!({"own": false, "text": true});
        assert_eq!(stored_after(&[(5, vec![nameless])], 0), []);
    }

    /// A placeholder ("could not be decrypted", a kind Kourier does not show) and a
    /// tombstone are rows from others with a new id, and neither is a message that arrived.
    #[test]
    fn a_row_without_content_is_not_an_arrived_message() {
        let placeholder = |message: i64| json!({"message": message, "own": false, "deleted": false, "text": false, "attachments": 0, "kinds": []});
        let tombstone = |message: i64, own: bool| json!({"message": message, "own": own, "deleted": true, "text": false, "attachments": 0});
        let photo = json!({"message": 33, "own": false, "deleted": false, "text": false, "attachments": 1, "kinds": ["image"]});
        assert!(has_content(&text_row(1, false)));
        assert!(has_content(&photo));
        assert!(!has_content(&placeholder(1)));
        assert!(!has_content(&tombstone(1, false)));
        // A deleted row has no content, whatever else it says.
        assert!(!has_content(
            &json!({"message": 1, "deleted": true, "text": true})
        ));
        // A row that says nothing about its content has none.
        assert!(!has_content(&json!({"message": 1, "own": false})));

        let chats = vec![
            (
                2,
                vec![
                    tombstone(34, true),
                    photo,
                    tombstone(32, false),
                    placeholder(31),
                    text_row(30, false),
                    text_row(25, false),
                ],
            ),
            (1, vec![placeholder(35), text_row(20, false)]),
        ];
        let new = stored_after(&chats, 25);
        assert_eq!(new, [stored(2, 2, 2), stored(1, 0, 1)]);
        assert_eq!(per_chat(&new, |s| s.arrived), [(2, 2)]);
        assert_eq!(per_chat(&new, |s| s.empty), [(2, 2), (1, 1)]);
        // Before the baseline they say nothing.
        assert_eq!(stored_after(&chats, 35), []);

        // The step fails on them, whatever it expected: two arrived, three did not, and a
        // count of all rows would have called that five.
        for expect in [2, 5] {
            let error = caught_up(&new, expect).expect_err("fails").to_string();
            assert!(
                error.contains("show no content: 3, [(2, 2), (1, 1)]"),
                "{error}"
            );
            assert!(
                error.contains(&format!(
                    "With content: 2 of the {expect} expected, [(2, 2)]"
                )),
                "{error}"
            );
        }
    }

    /// The verdict of a catch-up: every expected message arrived with its content, once.
    #[test]
    fn a_catch_up_passes_on_the_expected_count_only() {
        let new = [stored(1, 1, 0), stored(2, 2, 0)];
        assert_eq!(caught_up(&new, 3).expect("passes"), [(1, 1), (2, 2)]);
        assert_eq!(caught_up(&[], 0).expect("nothing expected"), []);
        let fewer = caught_up(&new, 4).expect_err("fails").to_string();
        assert!(
            fewer.contains("expected 4 new messages after the catch-up, found 3: [(1, 1), (2, 2)]"),
            "{fewer}"
        );
        assert!(fewer.contains("Fewer than expected"), "{fewer}");
        let more = caught_up(&new, 2).expect_err("fails").to_string();
        assert!(more.contains("found 3"), "{more}");
        assert!(more.contains("More than expected"), "{more}");
        let none = caught_up(&[], 3).expect_err("fails").to_string();
        assert!(none.contains("found 0: []"), "{none}");
    }

    /// One look at a chat shows one page. When every row of it is newer than the baseline
    /// and they may be a whole page, the chat may hold more: the count says so.
    #[test]
    fn a_page_of_new_rows_is_not_a_count() {
        let rows = |ids: std::ops::Range<i64>, own: bool| -> Vec<Value> {
            ids.rev().map(|id| text_row(id, own)).collect()
        };
        let page = i64::try_from(TIMELINE_PAGE).expect("small");
        let whole = i64::try_from(WHOLE_PAGE).expect("small");
        // The numbers the runbook states: a page of 50 rows, a whole page from 40 messages.
        assert_eq!((page, whole), (50, 40));
        let runbook =
            std::fs::read_to_string(root().join("docs/smoke-run.md")).expect("docs/smoke-run.md");
        for stated in [
            format!("{page} rows"),
            format!("{whole} or more"),
            format!("Up to {} new messages", whole - 1),
        ] {
            assert!(runbook.contains(&stated), "the runbook states {stated:?}");
        }
        // Fewer rows than a page can be: the chat holds no more (a chat that is new since
        // the baseline).
        assert_eq!(
            stored_after(&[(1, rows(101..100 + whole, false))], 100),
            [stored(1, whole - 1, 0)]
        );
        // A page of rows above the baseline, also when system rows took some of its places.
        for len in [whole, page] {
            assert_eq!(
                stored_after(&[(1, rows(101..101 + len, false))], 100),
                [Stored {
                    cut: true,
                    ..stored(1, len, 0)
                }]
            );
        }
        // The same page reaching back to the baseline's message shows everything newer.
        assert_eq!(
            stored_after(&[(1, rows(100..100 + page, false))], 100),
            [stored(1, page - 1, 0)]
        );
        // An own row from before the baseline proves the same.
        let mut reaching = rows(102..101 + page, false);
        reaching.push(text_row(99, true));
        assert_eq!(
            stored_after(&[(1, reaching)], 100),
            [stored(1, page - 1, 0)]
        );
        // Own rows fill a page too.
        assert_eq!(
            stored_after(&[(1, rows(101..101 + page, true))], 100),
            [Stored {
                cut: true,
                ..stored(1, 0, 0)
            }]
        );
        // Such a count is none: the step fails and names the chat, also when the number it
        // has is the expected one.
        let new = stored_after(
            &[(1, rows(101..101 + page, false)), (2, rows(90..110, false))],
            100,
        );
        assert!(whole_count(&new[1..]).is_ok());
        for error in [
            whole_count(&new).expect_err("no count"),
            caught_up(&new, page + 9).expect_err("no count"),
        ] {
            let error = error.to_string();
            assert!(error.contains("conversation #1 shows only rows"), "{error}");
            assert!(error.contains("Nothing was counted"), "{error}");
        }
        // The page is the app's.
        let source =
            std::fs::read_to_string(root().join("crates/kourier-viewmodel/src/core_source.rs"))
                .expect("the viewmodel's core source");
        assert!(
            source.contains(&format!("pub const TIMELINE_PAGE: u32 = {TIMELINE_PAGE};")),
            "TIMELINE_PAGE must be the app's page of timeline rows"
        );
    }

    /// The peer's modifiers are recognised by what they are (which message, which emoji),
    /// not by how many marks the chat holds.
    #[test]
    fn the_peers_marks_are_told_apart() {
        let row = |message: i64, own: bool| json!({"message": message, "own": own, "edited": false, "deleted": false, "quote": null, "reactions": []});
        let with = |mut row: Value, key: &str, value: Value| {
            row[key] = value;
            row
        };
        let chip = |emoji: &str, count: u64, mine: bool| json!({"emoji": emoji, "count": count, "mine": mine});
        let key = |message: i64, emoji: &str| (message, emoji.to_owned());
        let base_rows = vec![
            // Own rows carry no quote, edit or delete of the peer's.
            with(
                with(row(15, true), "quote", json!("known")),
                "edited",
                json!(true),
            ),
            with(row(14, false), "deleted", json!(true)),
            with(row(13, false), "edited", json!(true)),
            with(row(12, false), "quote", json!("known")),
            // Our reaction alone is no mark of the peer's.
            with(row(11, false), "reactions", json!([chip("👍", 1, true)])),
            with(row(10, true), "reactions", json!([chip("👍", 1, false)])),
        ];
        let base = peer_marks(&base_rows, 0);
        assert_eq!(
            base,
            [
                BTreeSet::from([key(10, "👍")]),
                BTreeSet::from([key(12, "")]),
                BTreeSet::from([key(13, "")]),
                BTreeSet::from([key(14, "")]),
            ]
        );
        assert_eq!(new_marks(&base, &base), [false; 4]);

        // The peer reacts to the same message with another emoji: Signal keeps one reaction
        // per person and message, so the chat holds as many marks as before.
        let mut rows = base_rows.clone();
        rows[5] = with(row(10, true), "reactions", json!([chip("🎉", 1, false)]));
        assert_eq!(
            new_marks(&base, &peer_marks(&rows, 0)),
            [true, false, false, false]
        );
        // The peer joins our reaction on a message of its own.
        let mut rows = base_rows.clone();
        rows[4] = with(row(11, false), "reactions", json!([chip("👍", 2, true)]));
        assert_eq!(
            new_marks(&base, &peer_marks(&rows, 0)),
            [true, false, false, false]
        );
        // A timer erased the rows that carried the base marks; one new reply with a quote
        // and one new tombstone bring the totals back to where they were.
        let rows = vec![
            with(row(21, false), "deleted", json!(true)),
            with(row(20, false), "quote", json!("known")),
        ];
        assert_eq!(
            new_marks(&base, &peer_marks(&rows, 0)),
            [false, true, false, true]
        );
        // Fewer marks than before and none of them new: nothing is seen.
        assert_eq!(
            new_marks(&base, &peer_marks(&base_rows[..3], 0)),
            [false; 4]
        );

        // Rows from before the base look (scrolled into view, or moved up after an erase)
        // are left out, so their old marks are not taken for new ones.
        let from = oldest_message(&base_rows);
        assert_eq!(from, 10);
        let mut rows = base_rows.clone();
        rows.push(with(
            with(row(7, false), "edited", json!(true)),
            "reactions",
            json!([chip("🎉", 1, false)]),
        ));
        assert_eq!(new_marks(&base, &peer_marks(&rows, from)), [false; 4]);
        assert_eq!(
            new_marks(&base, &peer_marks(&rows, 0)),
            [true, false, true, false]
        );
        assert_eq!(oldest_message(&[]), 0);
    }

    /// `receive` counts by unread counters, which a placeholder raises too: `step read`
    /// looks at the newest message from others of the chat it opens.
    #[test]
    fn a_placeholder_as_the_newest_message_is_seen() {
        let placeholder =
            json!({"message": 9, "own": false, "deleted": false, "text": false, "attachments": 0});
        let tombstone =
            json!({"message": 8, "own": false, "deleted": true, "text": false, "attachments": 0});
        let photo =
            json!({"message": 7, "own": false, "deleted": false, "text": false, "attachments": 1});
        assert!(placeholder_on_top(std::slice::from_ref(&placeholder)));
        // Own rows above it do not hide it.
        assert!(placeholder_on_top(&[
            text_row(11, true),
            placeholder.clone(),
            text_row(5, false)
        ]));
        // Only the newest message from others is judged: an older placeholder stays in a
        // chat for good.
        let rows = [text_row(10, false), placeholder.clone()];
        assert!(!placeholder_on_top(&rows));
        assert_eq!(newest_from_others(&rows), Some(&rows[0]));
        // A deleted message and a picture without a text are no placeholders.
        assert!(!placeholder_on_top(&[tombstone, placeholder.clone()]));
        assert!(!placeholder_on_top(&[photo, placeholder]));
        // Nothing from others, nothing to judge.
        assert!(!placeholder_on_top(&[text_row(3, true)]));
        assert!(!placeholder_on_top(&[]));
        assert_eq!(newest_from_others(&[text_row(3, true)]), None);
    }

    /// A received picture is told by its message: the media blocks of the chat are matched
    /// with its rows, and the number of shown pictures in the chat decides nothing.
    #[test]
    fn a_new_picture_is_told_by_its_message() {
        let row = |message: i64, own: bool, kinds: &[&str]| json!({"message": message, "own": own, "attachments": kinds.len(), "kinds": kinds});
        let block = |own: bool, block: &str, state: &str| json!({"own": own, "block": block, "state": state, "shown": state == "local" && block == "image"});
        let state = |rows: Vec<Value>, media: Vec<Value>| json!({"rows": rows, "media": media});
        let old = || row(20, false, &["image"]);
        let own = || row(29, true, &["image", "file"]);
        let own_media = || [block(true, "image", "local"), block(true, "file", "local")];
        let shows = |new: &str, older: &str| {
            let mut media = vec![block(false, "image", new)];
            media.extend(own_media());
            media.push(block(false, "image", older));
            state(vec![row(30, false, &["image"]), own(), old()], media)
        };
        let count = |state: &Value, after: i64| new_pictures(state, after).expect("lines up");
        // The new picture is there and not downloaded yet. The older one is not the new
        // picture, whenever its own download ends.
        assert_eq!(count(&shows("remote", "remote"), 25), (1, 0));
        assert_eq!(count(&shows("remote", "local"), 25), (1, 0));
        assert_eq!(count(&shows("downloading", "local"), 25), (1, 0));
        // The new one is shown.
        assert_eq!(count(&shows("local", "local"), 25), (1, 1));
        // A timer erased the older picture meanwhile: the chat shows as many as before.
        let erased = {
            let mut media = vec![block(false, "image", "local")];
            media.extend(own_media());
            state(vec![row(30, false, &["image"]), own()], media)
        };
        assert_eq!(count(&erased, 25), (1, 1));
        // Nothing new: not newer than the base look's newest message, or this account's own.
        assert_eq!(count(&shows("local", "local"), 30), (0, 0));
        let mut media = own_media().to_vec();
        media.push(block(false, "image", "local"));
        assert_eq!(count(&state(vec![own(), old()], media), 25), (0, 0));
        // An album counts each picture; a file, a clip and a sticker are no photo.
        let album = state(
            vec![
                row(32, false, &["file", "video", "sticker"]),
                row(31, false, &["image", "gif"]),
                row(30, false, &[]),
            ],
            vec![
                block(false, "file", "local"),
                block(false, "image", "local"),
                block(false, "image", "local"),
                block(false, "image", "remote"),
                block(false, "image", "local"),
            ],
        );
        assert_eq!(count(&album, 25), (2, 1));
        // The app caps the rows it lists and not their blocks: blocks of older rows are
        // left over.
        let mut capped = shows("local", "local");
        capped["media"]
            .as_array_mut()
            .expect("media")
            .push(block(true, "file", "local"));
        assert_eq!(count(&capped, 25), (1, 1));
        // Blocks that are not the rows' attachments: no count.
        let short = state(
            vec![row(30, false, &["image"]), own()],
            own_media().to_vec(),
        );
        let whose = state(
            vec![row(30, false, &["image"])],
            vec![block(true, "image", "local")],
        );
        let what = state(
            vec![row(30, false, &["image"])],
            vec![block(false, "file", "local")],
        );
        for broken in [short, whose, what] {
            let error = new_pictures(&broken, 25).expect_err("no count").to_string();
            assert!(error.contains("does not line up"), "{error}");
        }
        assert_eq!(count(&json!({}), 0), (0, 0));
    }

    /// The chats of an offline step are read in the list's order, the open one last: only
    /// a switch to a chat loads its newest page.
    #[test]
    fn the_open_chat_is_read_last() {
        let mut chats = vec![
            chat(4, "direct", 0),
            chat(2, "group", 1),
            chat(7, "other", 0),
            chat(3, "direct", 0),
            chat(5, "direct", 0),
        ];
        chats[4].is_self = true;
        let kinds = ["direct", "group"];
        assert_eq!(visit_order(&chats, &kinds, 0), [4, 2, 3]);
        assert_eq!(visit_order(&chats, &kinds, 4), [2, 3, 4]);
        assert_eq!(visit_order(&chats, &kinds, 2), [4, 3, 2]);
        assert_eq!(visit_order(&chats, &kinds, 3), [4, 2, 3]);
        // A chat that is open and not one of them (the note to self) changes nothing.
        assert_eq!(visit_order(&chats, &kinds, 5), [4, 2, 3]);
        assert_eq!(visit_order(&chats, &["group"], 4), [2]);
        assert_eq!(visit_order(&[], &kinds, 4), Vec::<i64>::new());
    }

    /// A baseline is taken only in the format this version writes.
    #[test]
    fn a_baseline_of_another_format_is_refused() {
        let written: Value =
            serde_json::from_str(&baseline(27, 3).to_string()).expect("the baseline is JSON");
        assert_eq!(baseline_newest(&written).expect("this version's"), 27);
        // The first format: "newest" was the newest id of the open chat alone.
        let first = json!({
            "chats": [{"conversation": 2, "kind": "direct", "self": false, "unread": 0}],
            "newest": 25,
            "open": 2,
        });
        assert!(baseline_newest(&first).is_err());
        assert!(baseline_newest(&json!({"newest": 25, "chats": 3})).is_err());
        assert!(baseline_newest(&json!({"v": 1, "newest": 25, "chats": 3})).is_err());
        assert!(baseline_newest(&json!({"v": BASELINE_VERSION, "chats": 3})).is_err());
    }

    fn shared(dir: &Path, local: bool) -> Shared {
        let dir = ProfileDir {
            dir: dir.to_owned(),
            keeper: false,
        };
        std::fs::create_dir_all(dir.smoke()).expect("smoke dir");
        Shared {
            dir,
            local,
            stdin: None,
            pending: HashMap::new(),
            next_id: 0,
            events: VecDeque::new(),
            next_seq: 0,
            qr_url: None,
            state: State::new("production", "production", 30, 0),
            dirty: false,
            link_in_flight: false,
            login_seen: false,
            ready: false,
            online: false,
            app_pid: 0,
            started_ms: 0,
        }
    }

    fn app_event(name: &str, data: Value) -> Value {
        json!({"type": "event", "event": name, "data": data})
    }

    /// The QR link never reaches a file or a client in a run with a real server; the mock's
    /// phone gets it from memory. The login's progress and end are followed.
    #[test]
    fn the_supervisor_keeps_the_link_to_itself() {
        const LINK: &str = "sgnl://linkdevice?uuid=abc&pub_key=BQxyz";
        let scratch = Scratch::new("events");
        let mut real = shared(&scratch.0.join("real"), false);
        real.on_app_event(app_event("channel", json!({"protocol": 1})));
        assert!(real.ready);
        real.on_app_event(app_event("login_qr", json!({"account": 1, "url": LINK})));
        assert_eq!(real.qr_url, None);
        assert_eq!(
            real.events.back().expect("event").1["data"],
            json!({"account": 1, "shown": true})
        );
        let written = std::fs::read_to_string(real.dir.events_log()).expect("events file");
        assert!(
            !written.contains("sgnl://") && written.contains("login_qr"),
            "{written}"
        );
        let mut mock = shared(&scratch.0.join("mock"), true);
        mock.on_app_event(app_event("login_qr", json!({"account": 1, "url": LINK})));
        assert_eq!(mock.qr_url.as_deref(), Some(LINK));
        assert!(
            !std::fs::read_to_string(mock.dir.events_log())
                .expect("events file")
                .contains("sgnl://")
        );

        // From the phone's answer to the end of the login a stop is refused.
        real.on_app_event(app_event(
            "login_step",
            json!({"account": 1, "kind": "progress"}),
        ));
        assert!(real.link_in_flight && !real.state.linked());
        real.on_app_event(app_event(
            "login_step",
            json!({"account": 1, "kind": "done"}),
        ));
        assert!(!real.link_in_flight && real.state.linked() && real.dirty);
        real.on_app_event(app_event(
            "account_state",
            json!({"account": 1, "state": "active", "conn": "online"}),
        ));
        assert!(real.online && real.state.num("last_online_ms").is_some());
        real.on_app_event(app_event(
            "account_state",
            json!({"account": 1, "state": "active", "conn": "offline"}),
        ));
        assert!(!real.online && real.state.linked());
        // An error's text is redacted on its way into the record.
        real.on_app_event(app_event(
            "error",
            json!({"source": "account", "message": "peer 9d0652a3-dcc3-4d11-975f-74d61598733f"}),
        ));
        assert_eq!(
            real.events.back().expect("event").1["data"]["message"],
            "peer …733f"
        );
        // The server dropped the device: the login has ended, and the tool knows.
        real.on_app_event(app_event(
            "account_state",
            json!({"account": 1, "state": "logged_out", "conn": "logged_out"}),
        ));
        assert!(!real.state.linked() && real.state.num("logged_out_ms").is_some());
    }

    /// A state file that lost "linked" (the supervisor was killed between the end of a login
    /// and its next save) learns it again from an account that is simply online; an account
    /// that comes online inside a login of this run waits for the login's end.
    #[test]
    fn a_linked_profile_is_recognised_without_its_login() {
        let scratch = Scratch::new("adopt");
        let online = || {
            app_event(
                "account_state",
                json!({"account": 1, "state": "active", "conn": "online", "device": 2}),
            )
        };
        let mut resumed = shared(&scratch.0.join("resumed"), false);
        assert!(!resumed.state.linked());
        resumed.on_app_event(online());
        assert!(resumed.state.linked() && resumed.dirty);
        assert_eq!(resumed.state.num("device_id"), Some(2));

        let mut linking = shared(&scratch.0.join("linking"), false);
        linking.on_app_event(app_event(
            "login_step",
            json!({"account": 1, "kind": "progress"}),
        ));
        linking.on_app_event(online());
        assert!(!linking.state.linked(), "the login has not ended yet");
        linking.on_app_event(app_event(
            "login_step",
            json!({"account": 1, "kind": "done"}),
        ));
        assert!(linking.state.linked());
    }

    /// The supervisor starts the app only on a fresh order written by `start`, once: run by
    /// hand it finds none (or a used, stale or foreign one) and starts nothing, so no real
    /// server is reached around `--confirmed-by-user`.
    #[test]
    fn a_launch_order_starts_the_app_once() {
        let scratch = Scratch::new("launch");
        let dir = ProfileDir {
            dir: scratch.0.clone(),
            keeper: false,
        };
        private_dir(&dir.smoke()).expect("dirs");
        let state = State::new("production", "production", 30, 0);
        let now = 1_000_000_000;
        let order = |env: &str, issued: u64| {
            write_private(
                &dir.launch_file(),
                &json!({"bin": "/bin/kourier", "env_arg": env, "issued_ms": issued}).to_string(),
            )
            .expect("launch.json");
        };
        // No order: the supervisor was started by hand.
        let error = take_launch(&dir, &state, now).expect_err("nothing to start");
        assert!(error.to_string().contains("no launch order"), "{error}");
        // A fresh order is taken once.
        order("production", now - 500);
        let launch = take_launch(&dir, &state, now).expect("a fresh order");
        assert_eq!(launch["env_arg"], "production");
        assert!(!dir.launch_file().exists(), "the order is used up");
        assert!(take_launch(&dir, &state, now).is_err());
        // An old order, one from the future, one for another server than the profile's, and
        // one without a server: refused, and gone.
        for (env, issued) in [
            ("production", now - LAUNCH_VALID_MS - 1),
            ("production", now + LAUNCH_VALID_MS + 1),
            ("staging", now),
        ] {
            order(env, issued);
            assert!(take_launch(&dir, &state, now).is_err(), "{env} {issued}");
            assert!(!dir.launch_file().exists());
        }
        write_private(&dir.launch_file(), &json!({"issued_ms": now}).to_string()).expect("file");
        assert!(take_launch(&dir, &state, now).is_err());
        // The app's command never falls back to a server of its own choice.
        assert!(app_command(Path::new("/bin/kourier"), &dir, &json!({"local": false})).is_err());
        assert!(app_command(Path::new("/bin/kourier"), &dir, &json!({"env_arg": ""})).is_err());
    }

    /// A 1:1 send goes out only with the timer the user confirmed from the phone: unknown,
    /// different, changed since, or never compared all mean that nothing is sent.
    #[test]
    fn a_send_to_the_peer_needs_the_confirmed_timer() {
        // Given on this step: it must equal what Kourier shows.
        assert_eq!(timer_to_send_with(Some(0), Some(0), None).expect("off"), 0);
        assert_eq!(
            timer_to_send_with(Some(604_800), Some(604_800), Some(0)).expect("a week"),
            604_800
        );
        let differs = timer_to_send_with(Some(604_800), Some(0), None).expect_err("differs");
        assert!(
            differs.to_string().contains("nothing was sent"),
            "{differs}"
        );
        assert!(timer_to_send_with(Some(0), Some(3600), Some(0)).is_err());
        // Confirmed by an earlier step: it must still be what Kourier shows.
        assert_eq!(
            timer_to_send_with(Some(30), None, Some(30)).expect("same"),
            30
        );
        let changed = timer_to_send_with(Some(30), None, Some(0)).expect_err("changed");
        assert!(changed.to_string().contains("--timer"), "{changed}");
        // Never compared.
        let never = timer_to_send_with(Some(0), None, None).expect_err("never compared");
        assert!(never.to_string().contains("was not compared"), "{never}");
        // Kourier does not know the chat's timer yet (its header is not loaded): "off" is
        // never assumed, whatever the phone says.
        for (given, confirmed) in [(Some(0), None), (None, Some(0)), (Some(0), Some(0))] {
            assert!(timer_to_send_with(None, given, confirmed).is_err());
        }
        // What the app's state says about the open chat.
        assert_eq!(chat_timer(&json!({"open": 3, "timer_secs": null})), None);
        assert_eq!(chat_timer(&json!({"open": 3})), None);
        assert_eq!(chat_timer(&json!({"timer_secs": "0"})), None);
        assert_eq!(chat_timer(&json!({"timer_secs": -1})), None);
        assert_eq!(chat_timer(&json!({"timer_secs": 0})), Some(0));
        assert_eq!(chat_timer(&json!({"timer_secs": 3600})), Some(3600));
    }

    /// The remembered chats belong to one database file: a rebuilt or restored `core.db` may
    /// number its chats differently, so they are forgotten rather than sent to.
    #[test]
    fn remembered_chats_do_not_outlive_their_database() {
        let scratch = Scratch::new("chats-db");
        let db = scratch.0.join("core.db");
        std::fs::write(&db, "one").expect("core.db");
        let id = file_id(&db).expect("an id");
        let remembering = |chats_db: Value| {
            let mut state = State::new("production", "production", 30, 0);
            state.set("peer_conv", json!(2));
            state.set("group_conv", json!(1));
            state.set("peer_timer_secs", json!(0));
            state.set("chats_db", chats_db);
            state
        };
        // The same file: everything stays.
        let mut state = remembering(json!(id));
        assert!(!forget_chats_of_another_db(&mut state, &db));
        assert_eq!(state.0["peer_conv"], 2);
        // Another file in its place (a rebuild, a restored copy): forgotten.
        std::fs::rename(&db, scratch.0.join("old.db")).expect("set aside");
        std::fs::write(&db, "two").expect("a new core.db");
        assert_ne!(file_id(&db), Some(id.clone()));
        let mut state = remembering(json!(id));
        assert!(forget_chats_of_another_db(&mut state, &db));
        for key in ["peer_conv", "group_conv", "peer_timer_secs", "chats_db"] {
            assert!(state.0[key].is_null(), "{key}");
        }
        // No database at all, or chats remembered without their database: forgotten too.
        std::fs::remove_file(&db).expect("remove");
        assert_eq!(file_id(&db), None);
        assert!(forget_chats_of_another_db(&mut remembering(json!(id)), &db));
        std::fs::write(&db, "three").expect("core.db");
        assert!(forget_chats_of_another_db(
            &mut remembering(Value::Null),
            &db
        ));
        // Nothing remembered: nothing to forget.
        let mut fresh = State::new("production", "production", 30, 0);
        assert!(!forget_chats_of_another_db(&mut fresh, &db));

        // The supervisor notes the database with every chat it is told to remember.
        let profile = scratch.0.join("profile");
        let shared = Arc::new(Mutex::new(shared(&profile, false)));
        let core_db = lock(&shared).dir.core_db();
        std::fs::create_dir_all(core_db.parent().expect("data dir")).expect("data dir");
        std::fs::write(&core_db, "db").expect("core.db");
        assert_eq!(
            handle(&shared, &json!({"op": "set", "peer_conv": 2}))["ok"],
            true
        );
        let mut noted = lock(&shared).state.clone();
        assert_eq!(noted.0["peer_conv"], 2);
        assert_eq!(noted.str("chats_db").map(str::to_owned), file_id(&core_db));
        assert!(!forget_chats_of_another_db(&mut noted, &core_db));

        // A remembered chat must still be a chat of its kind in the list.
        let chats = vec![chat(1, "group", 0), chat(2, "direct", 0), {
            let mut me = chat(3, "direct", 0);
            me.is_self = true;
            me
        }];
        assert!(still_the_chat(&chats, 2, "direct") && still_the_chat(&chats, 1, "group"));
        assert!(
            !still_the_chat(&chats, 1, "direct"),
            "a group is no peer chat"
        );
        assert!(!still_the_chat(&chats, 2, "group"));
        assert!(
            !still_the_chat(&chats, 3, "direct"),
            "the note to self is no peer"
        );
        assert!(!still_the_chat(&chats, 9, "direct"), "gone from the list");
    }

    /// No send while a challenge stands, while the account is not online, or while files an
    /// earlier step left in the composer would ride along.
    #[test]
    fn nothing_is_sent_into_a_state_that_would_send_more_or_in_vain() {
        let state = |challenge: Value, accounts: Value, composer: Value| json!({"challenge": challenge, "accounts": accounts, "composer": composer});
        let empty = || json!({"files": 0, "sendable": 0});
        assert_eq!(
            send_blocker(&state(Value::Null, json!(["online"]), empty())),
            None
        );
        let challenged = send_blocker(&state(json!(1), json!(["online"]), empty()));
        assert!(challenged.is_some_and(|why| why.contains("428")));
        for accounts in [
            json!(["syncing"]),
            json!([]),
            json!(["online", "online"]),
            json!(["logged_out"]),
        ] {
            let why = send_blocker(&state(Value::Null, accounts, empty()));
            assert!(why.is_some_and(|why| why.contains("not online")));
        }
        let left = send_blocker(&state(Value::Null, json!(["online"]), json!({"files": 2})));
        assert!(
            left.is_some_and(|why| why.contains("2 file(s)") && why.contains("nothing was sent"))
        );
        // A state that does not say what the composer holds does not count as empty.
        for composer in [Value::Null, json!({}), json!({"files": "0"})] {
            let why = send_blocker(&state(Value::Null, json!(["online"]), composer));
            assert!(why.is_some_and(|why| why.contains("composer")));
        }
    }

    /// The send cap: a sending command says how many messages it sends, the count is taken
    /// before the command leaves, and past the cap nothing leaves.
    #[test]
    fn the_send_cap_holds() {
        assert!(sends_messages("send") && sends_messages("act"));
        for free in [
            "open_id",
            "chats",
            "state",
            "wait",
            "attach",
            "page",
            "add_account",
            "submit_login",
            "quit",
        ] {
            assert!(!sends_messages(free), "{free}");
        }
        let scratch = Scratch::new("cap");
        let mut s = shared(&scratch.0, false);
        s.state.set("max_sends", json!(2));
        s.state.set("sends_used", json!(1));
        let shared = Arc::new(Mutex::new(s));
        let ask = |sends: u64| {
            ctl(
                &shared,
                &json!({"op": "ctl", "cmd": "send", "args": {"text": "x"}, "sends": sends, "timeout_ms": 10}),
            )
        };
        // A send that does not say how many, and one past the cap: refused, nothing counted.
        assert_eq!(ask(0)["error"]["code"], "budget");
        assert_eq!(ask(2)["error"]["code"], "budget");
        assert_eq!(lock(&shared).state.num("sends_used"), Some(1));
        // Within the cap: counted before it leaves (the app is not there in this test).
        assert_eq!(ask(1)["error"]["code"], "gone");
        assert_eq!(lock(&shared).state.num("sends_used"), Some(2));
        assert_eq!(ask(1)["error"]["code"], "budget");
        // A command that sends nothing needs no budget.
        let free = ctl(&shared, &json!({"op": "ctl", "cmd": "state"}));
        assert_eq!(free["error"]["code"], "gone");
        // Commands are recorded by name, never with their arguments.
        let record =
            std::fs::read_to_string(scratch.0.join("smoke/events.jsonl")).unwrap_or_default();
        assert!(!record.contains("\"text\""), "{record}");
    }

    #[test]
    fn the_app_runs_scrubbed_and_guarded() {
        let dir = ProfileDir {
            dir: PathBuf::from("/p"),
            keeper: true,
        };
        let envs = |launch: Value| -> HashMap<String, Option<String>> {
            app_command(Path::new("/bin/kourier"), &dir, &launch)
                .expect("a launch order with a server")
                .get_envs()
                .map(|(k, v)| {
                    (
                        k.to_string_lossy().into_owned(),
                        v.map(|v| v.to_string_lossy().into_owned()),
                    )
                })
                .collect()
        };
        let real = envs(json!({"env_arg": "production", "local": false, "expect_tail": "1234"}));
        let set = |e: &HashMap<String, Option<String>>, k: &str| e.get(k).cloned().flatten();
        assert_eq!(set(&real, "KOURIER_HOME").as_deref(), Some("/p"));
        assert_eq!(
            set(&real, "KOURIER_SIGNAL_ENV").as_deref(),
            Some("production")
        );
        assert_eq!(
            set(&real, "KOURIER_DEV_KEYRING").as_deref(),
            Some("passphrase")
        );
        assert_eq!(set(&real, "KOURIER_DEV_NO_DUMP").as_deref(), Some("1"));
        assert_eq!(set(&real, "KOURIER_DEV_QR_URL").as_deref(), Some("0"));
        assert_eq!(set(&real, "KOURIER_DESKTOP").as_deref(), Some("0"));
        assert_eq!(
            set(&real, "KOURIER_DEV_EXPECT_NUMBER_TAIL").as_deref(),
            Some("1234")
        );
        assert_eq!(set(&real, "KOURIER_LOG").as_deref(), Some(LOG_FILTER));
        assert_eq!(
            set(&real, "KOURIER_HISTORY_TRANSFER"),
            None,
            "the history transfer is offered only when the launch order says so"
        );
        let transfer =
            envs(json!({"env_arg": "production", "local": false, "history_transfer": true}));
        assert_eq!(
            set(&transfer, "KOURIER_HISTORY_TRANSFER").as_deref(),
            Some("1")
        );
        assert_eq!(set(&real, "QT_QPA_PLATFORM"), None, "a window by default");
        // The filter never opens libsignal's protocol log and never is a bare level.
        assert!(LOG_FILTER.contains("libsignal_protocol=warn") && LOG_FILTER.starts_with("info,"));
        assert!(LOG_FILTER.contains("kourier_signal::requests=debug"));
        let mock = envs(
            json!({"env_arg": "local:/x/hs.json", "local": true, "offscreen": true, "desktop": true}),
        );
        assert_eq!(
            set(&mock, "KOURIER_DEV_QR_URL"),
            None,
            "the mock phone needs the link"
        );
        assert_eq!(set(&mock, "KOURIER_DESKTOP"), None);
        assert_eq!(set(&mock, "QT_QPA_PLATFORM").as_deref(), Some("offscreen"));
        assert_eq!(set(&mock, "KOURIER_DEV_EXPECT_NUMBER_TAIL"), None);
    }

    /// The step list and the runbook agree, and only flagged steps end the login.
    #[test]
    fn the_steps() {
        let names: Vec<&str> = STEPS.iter().map(|d| d.name).collect();
        let mut unique = names.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), names.len());
        assert_eq!(names[0], "link");
        let ending: Vec<&str> = STEPS
            .iter()
            .filter(|d| d.destroys_login)
            .map(|d| d.name)
            .collect();
        assert_eq!(ending, ["unlink-from-phone"]);
        let default_sends: u64 = STEPS
            .iter()
            .filter(|d| !d.destroys_login)
            .map(|d| d.sends)
            .sum();
        assert!(
            default_sends <= DEFAULT_MAX_SENDS,
            "the default run fits its cap: {default_sends}"
        );
        let runbook =
            std::fs::read_to_string(root().join("docs/smoke-run.md")).expect("docs/smoke-run.md");
        for name in &names {
            assert!(
                runbook.contains(&format!("step {name}")),
                "the runbook does not name `step {name}`"
            );
        }
        assert!(runbook.contains(&format!("{IDLE_DEADLINE_DAYS} days")));
        assert!(!runbook.contains('\u{2014}'), "no em dashes in the runbook");
        // A step that ends the login does nothing without its flag.
        let a = args(&[
            "step",
            "unlink-from-phone",
            "--home",
            "/nonexistent/profile",
        ])
        .expect("parses");
        assert_eq!(step(&a).expect("refused, not an error"), 1);
        assert!(step(&args(&["step", "no-such-step"]).expect("parses")).is_err());
    }

    #[test]
    fn the_group_send_line_tells_how_it_went_out() {
        let log = "INFO kourier_signal::send::group: group message sent group=2be66890 request=\"Message\" delivered=1 failed=0 sender_key=0\n\
                   DEBUG other\n\
                   INFO kourier_signal::send::group: group message sent group=2be66890 request=\"Message\" delivered=2 failed=0 sender_key=2\n";
        assert_eq!(sender_key_recipients(log), Some(2));
        assert_eq!(
            sender_key_recipients(log.lines().next().unwrap_or_default()),
            Some(0)
        );
        assert_eq!(sender_key_recipients("nothing about a group"), None);
    }

    #[test]
    fn timers_and_tails() {
        assert_eq!(parse_timer("off").expect("off"), 0);
        assert_eq!(parse_timer("0").expect("0"), 0);
        assert_eq!(parse_timer("3600").expect("s"), 3600);
        assert!(parse_timer("1h").is_err() && parse_timer("-5").is_err());
        assert_eq!(
            (timer_text(0), timer_text(30)),
            ("off".to_owned(), "30 s".to_owned())
        );
        assert!(valid_tail("1234") && valid_tail("00") && !valid_tail("1") && !valid_tail("12a4"));
        assert!(
            valid_profile_name("default") && !valid_profile_name("a b") && !valid_profile_name("")
        );
    }

    #[test]
    fn the_test_picture_is_a_png() {
        let png = test_png(96, 64);
        assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
        assert_eq!(&png[12..16], b"IHDR");
        assert_eq!(&png[16..24], &[0, 0, 0, 96, 0, 0, 0, 64]);
        assert_eq!(&png[png.len() - 8..png.len() - 4], b"IEND");
        // The CRC of an empty IEND chunk is a known value.
        assert_eq!(&png[png.len() - 4..], &[0xAE, 0x42, 0x60, 0x82]);
        assert_eq!(base64(b"Man"), "TWFu");
        assert_eq!(base64(b"Ma"), "TWE=");
        assert_eq!(base64(b"M"), "TQ==");
    }

    #[test]
    fn the_report_lists_every_step() {
        let dir = ProfileDir {
            dir: PathBuf::from("/p"),
            keeper: true,
        };
        let mut state = State::new("production", "production", 30, 0);
        state.set("sends_used", json!(3));
        let steps = vec![
            json!({"name": "link", "result": "PASS", "reason": "", "started_ms": 0, "ended_ms": 41_300, "sends": 0, "notes": ["online"], "log": {"from": 0, "to": 10}}),
            json!({"name": "send", "result": "FAIL", "reason": "the timer differs | nothing was sent", "started_ms": 60_000, "ended_ms": 61_000, "sends": 0, "notes": [], "log": {"from": 10, "to": 20}}),
        ];
        let text = report_text(&dir, &state, &steps, 120_000);
        assert!(
            text.contains("| 1 | link | PASS | 1970-01-01 00:00:00 UTC | 41.3 s | 0 |"),
            "{text}"
        );
        assert!(
            text.contains("| 2 | send | FAIL |")
                && text.contains("the timer differs / nothing was sent")
        );
        assert!(text.contains("- Steps: 2 run, 1 passed, 1 failed"));
        assert!(text.contains("- Messages sent by the tool: 3 of 30"));
        assert!(text.contains("| passive | not run | 0 |") && text.contains("| send | FAIL | 1 |"));
        assert!(text.contains("- online\n"));
    }

    /// The cause of the flake of the test below: threads that start programs carry the lock
    /// of a profile into their children for a moment. Four threads start `true` all the time;
    /// a lock taken, dropped and taken again must still always be free. (With a single ask
    /// instead of [`try_lock_for`] about a third of the second locks were refused.)
    #[test]
    fn a_lock_that_a_child_about_to_exec_still_carries_is_not_the_running_apps() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let scratch = Scratch::new("lock-carried");
        let profile = ProfileDir {
            dir: scratch.0.join("profile"),
            keeper: false,
        };
        private_dir(&profile.smoke()).expect("dirs");
        let stop = Arc::new(AtomicBool::new(false));
        let spawners: Vec<_> = (0..4)
            .map(|_| {
                let stop = stop.clone();
                std::thread::spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        let _ = Command::new("true").status();
                    }
                })
            })
            .collect();
        for round in 0..300 {
            let held = OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(profile.lock_file())
                .expect("lock file");
            assert!(
                try_lock_for(&held, LOCK_SETTLE).expect("lock"),
                "round {round}"
            );
            std::thread::sleep(Duration::from_micros(300));
            drop(held);
            assert!(
                lock_is_free_settled(&profile).expect("check"),
                "round {round}"
            );
        }
        stop.store(true, Ordering::Relaxed);
        for t in spawners {
            t.join().expect("spawner");
        }
    }

    /// A lock that is held for good is still reported, after the wait and not before.
    #[test]
    fn a_lock_that_is_held_for_good_is_refused_after_the_wait() {
        let scratch = Scratch::new("lock-held");
        let path = scratch.0.join("lock");
        let open = || {
            OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(&path)
                .expect("lock file")
        };
        let holder = open();
        holder.try_lock().expect("held");
        let other = open();
        let started = Instant::now();
        assert!(!try_lock_for(&other, Duration::from_millis(150)).expect("asked"));
        assert!(started.elapsed() >= Duration::from_millis(150));
        // One that is released while it is waited for is taken.
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            drop(holder);
        });
        assert!(try_lock_for(&other, Duration::from_secs(5)).expect("asked"));
        releaser.join().expect("releaser");
    }

    /// The wipe and the backup act only on a profile of this tool.
    #[test]
    fn a_directory_that_is_no_smoke_profile_is_never_deleted() {
        let scratch = Scratch::new("wipe");
        let stranger = scratch.0.join("stranger");
        std::fs::create_dir_all(stranger.join("precious")).expect("dir");
        let home = stranger.to_string_lossy().into_owned();
        let a = args(&["wipe", "--home", &home, "--yes-delete-the-login"]).expect("parses");
        assert!(wipe(&a).is_err());
        assert!(stranger.join("precious").exists());
        // A profile of the tool: nothing without the flag, everything with it.
        let profile = ProfileDir {
            dir: scratch.0.join("profile"),
            keeper: false,
        };
        private_dir(&profile.smoke()).expect("dirs");
        std::fs::write(profile.dir.join("data"), "x").expect("file");
        State::new("local:localhost:1", "local:/x", 30, 0)
            .save(&profile)
            .expect("state");
        let home = profile.dir.to_string_lossy().into_owned();
        assert_eq!(
            wipe(&args(&["wipe", "--home", &home]).expect("parses")).expect("kept"),
            1
        );
        assert!(profile.dir.join("data").exists());
        // A copy first: the whole directory, private, without the app's run directory.
        std::fs::create_dir_all(profile.dir.join("kourier/run/media")).expect("run");
        std::fs::write(profile.dir.join("kourier/run/media/plain"), "x").expect("file");
        let to = scratch.0.join("copies");
        std::fs::create_dir_all(&to).expect("dir");
        let to_arg = to.to_string_lossy().into_owned();
        assert_eq!(
            backup(&args(&["backup", "--home", &home, "--to", &to_arg]).expect("parses"))
                .expect("copied"),
            0
        );
        let copy = std::fs::read_dir(&to)
            .expect("copies")
            .flatten()
            .next()
            .expect("one copy")
            .path();
        assert!(copy.join("data").exists() && copy.join("smoke/state.json").exists());
        assert!(!copy.join("kourier/run").exists());
        assert_eq!(
            std::fs::metadata(&copy).expect("meta").permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            wipe(&args(&["wipe", "--home", &home, "--yes-delete-the-login"]).expect("parses"))
                .expect("deleted"),
            0
        );
        assert!(!profile.dir.exists() && copy.join("data").exists());
    }
}
