//! `threema-run start`: what happens after the preflight has passed, up to a supervisor that
//! answers (`briefs/tr.md` §2.3, §2.5, §2.7 and the critic's R-H1, C7, M3).
//!
//! The order of `start_in` (nothing is made before the preflight's last check):
//!
//! 1. the profile's directories (0700; the state file only when the profile has none) and the
//!    control socket's directory;
//! 2. a leftover list of test IDs removed (the file's life, path 3);
//! 3. the stamped, execute-only copy of the app (built first unless `--no-build`);
//! 4. the open question of a failed link, when the state says a login stood at `linking`;
//! 5. THE SWITCH ON, held by a [`SwitchGuard`] from this write on: every refusal or failure up to
//!    the moment the supervisor answers puts `networks-on = []` back (critic M3); after the
//!    supervisor was started it does so only when that supervisor is gone (a switch written off
//!    while the app still runs can be written back by the app's own preference save);
//! 6. the probe: the stamped app over `--print-networks`, which must name Threema as registered
//!    (the switch took) while no list of test IDs stands in the keeper root;
//! 7. the state (the halt moved, the start counted, the commit), the launch order (`launch.json`),
//!    the supervisor, detached; the wait for its first answer.
//!
//! The supervisor's body is T3b's ([`super::supervise`]). It holds the profile lock until the
//! app's process is gone (the guard's "is it gone" is that lock being free) and writes the switch
//! off at every end ([`switch_off`]).
//!
//! The control socket is `<XDG_RUNTIME_DIR>/kthr-<16 hex>.sock`, the hex the first 8 bytes of the
//! SHA-256 of the profile's path: 41 bytes for ANY checkout and profile (critic C7: the place never
//! depends on the checkout's length). A rehearsal's socket directory is `/dev/shm/kthr-<uid>-<secs>`
//! (the `<secs>` is its place's own).
//!
//! The app's environment is a LIST that a test pins whole, for the real order and the rehearsal's
//! (critic R-H1): the profile, the development keyring, no core dump, no QR link in events, no
//! desktop services (no notification with a name or a text, no tray, no bus name), Qt's log on
//! stderr, no audio server, the log filter, and every inherited `KOURIER_*` variable taken out.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::OpenOptions;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

use super::preflight::{
    Cleared, Ctx, NETWORK_NAME, SOCKET_NAME_LEN, SOCKET_PATH_MAX, check_socket_place, check_source,
    preflight, switch_text, with_real_ctx,
};
use super::profile::{
    ProfileDir, State, check_canonical, open_lock_file, private_dir, read_json,
    remove_leftover_list, try_lock_for, write_private,
};
use super::{Args, REHEARSAL_DIR};

/// The app's log filter: Kourier's own crates at INFO (libthreema stays at WARN whatever this
/// says: `.claude/rules/threema-app.md` THE LOG RULE).
pub(super) const APP_LOG_FILTER: &str =
    "info,kourier_threema=info,kourier_core=info,kourier_app=info";
/// No sound: a program that makes one finds no audio server (`CLAUDE.md` "Automated runs are
/// silent"; the real run is silent too, the window is what the user needs).
const NO_AUDIO_SERVER: &str = "unix:/nonexistent/kourier-threema-run-no-audio";
const NO_PIPEWIRE: &str = "kourier-threema-run-no-audio";
/// The app's seam for the fake that plays Threema (T8) and the word it takes.
pub(super) const REHEARSAL_ENV: &str = "KOURIER_DEV_REHEARSAL";
pub(super) const REHEARSAL_WORD: &str = "threema";
/// How long a launch order of `start` stays valid: the supervisor reads it within a moment.
const LAUNCH_VALID_MS: u64 = 120_000;
/// How long the profile's lock is asked for again while it looks held (a program another thread is
/// about to `exec` carries a copy of the descriptor: `.claude/rules/testing.md`).
pub(super) const LOCK_SETTLE: Duration = Duration::from_secs(1);
/// The directory of a rehearsal's control socket (a RAM file system; a socket file costs nothing).
pub(super) const REHEARSAL_SOCKET_BASE: &str = "/dev/shm";
/// The word of the supervisor's `info` that says it waits for the ID (`id`).
pub(super) const INFO_ID_WAITING: &str = "waiting";
/// The kind word of the open question a start opens when a login stood at `linking` (stop rule 1b).
pub(super) const QUESTION_KIND: &str = "link-failed-after-hello";
/// The words of a refusal made after the switch was written, for a supervisor that may live.
const SWITCH_LEFT_ON: &str = "warning: the supervisor may still be alive, so Threema stays switched ON in the profile until it ends: run `threema-run stop` before anything else.";

fn short(rev: &str) -> &str {
    &rev[..rev.len().min(10)]
}

// ---- The profile's launch files ---------------------------------------------------------------------

impl ProfileDir {
    /// The stamped copy of the app and its stamp.
    pub fn bin_dir(&self) -> PathBuf {
        self.tool().join("bin")
    }
    pub fn copy_file(&self) -> PathBuf {
        self.bin_dir().join("kourier")
    }
    pub fn stamp_file(&self) -> PathBuf {
        self.bin_dir().join("stamp.json")
    }
    /// The order `start` leaves for the supervisor (taken away when it is read).
    pub fn launch_file(&self) -> PathBuf {
        self.tool().join("launch.json")
    }
}

// ---- Hashing ----------------------------------------------------------------------------------------

/// `program` as a command whose process gets no core file: `RLIMIT_CORE` is 0 before the exec, so a
/// crash of it cannot hand what it holds to a core handler (LOW-8). Nothing else is set.
pub(super) fn no_core_command(program: &str) -> Command {
    let mut cmd = Command::new(program);
    // SAFETY: only `setrlimit` runs between fork and exec; it is async-signal-safe and takes a
    // plain struct.
    unsafe {
        cmd.pre_exec(|| {
            let none = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            if libc::setrlimit(libc::RLIMIT_CORE, &none) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    cmd
}

/// `sha256` of `bytes` as 64 lower-case hex digits, by `sha256sum` on stdin (xtask has no hash
/// crate; tg_run/preflight.rs:305 `sha256_bytes`). Whatever is hashed is in that pipe and process for
/// milliseconds: T4's `id` takes this function too. LOW-8: the child gets no core file
/// (`RLIMIT_CORE` 0 before the exec, so a crash of it cannot hand the pipe's bytes to a core
/// handler), and the text it printed (the digest) is wiped in this process before the return.
pub(super) fn sha256_bytes(bytes: &[u8]) -> Result<String> {
    let mut cmd = no_core_command("sha256sum");
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped());
    let mut child = cmd.spawn().context("spawn sha256sum")?;
    child
        .stdin
        .take()
        .context("sha256sum stdin")?
        .write_all(bytes)?;
    let mut out = child.wait_with_output()?;
    let result = (|| {
        if !out.status.success() {
            bail!("sha256sum failed");
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let sum = text.split_whitespace().next().unwrap_or_default();
        if sum.len() != 64 || !sum.bytes().all(|b| b.is_ascii_hexdigit()) {
            bail!("sha256sum printed no hash");
        }
        Ok(sum.to_ascii_lowercase())
    })();
    // The digest's text in the child's output buffer is as good as the ID: no copy stays behind.
    super::prompt::wipe(&mut out.stdout);
    result
}

/// The SHA-256 of a file, by `sha256sum` (wa_run.rs:1917 `sha256_of`).
fn sha256_of(path: &Path) -> Result<String> {
    let out = Command::new("sha256sum")
        .arg("--")
        .arg(path)
        .output()
        .context("run sha256sum")?;
    if !out.status.success() {
        bail!("sha256sum failed on a file");
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let sum = text.split_whitespace().next().unwrap_or_default();
    if sum.len() != 64 || !sum.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("sha256sum printed no hash for a file");
    }
    Ok(sum.to_ascii_lowercase())
}

// ---- The stamped copy -------------------------------------------------------------------------------

/// What a stamped copy of the app is, and what it was made from (wa_run.rs:1826 `Stamp`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Stamp {
    /// main's HEAD the binary was built at; `None` for the rehearsal's copy.
    pub commit: Option<String>,
    /// The tree was clean when it was built (a real run's copy always; the rehearsal's never).
    pub clean: bool,
    /// HEAD's migration files with their blob ids (what the supervisor records as having run once
    /// the app opened its databases).
    pub migrations: BTreeMap<String, String>,
    pub sha256: String,
    /// `debug`, `release` or `other`, from the directory the binary stood in.
    pub build: Option<String>,
    /// Identity of the copy: its size, modification time and inode when it was stamped.
    pub size: u64,
    pub mtime_ns: u64,
    pub inode: u64,
}

impl Stamp {
    fn to_json(&self, made_ms: u64) -> Value {
        json!({
            "commit": self.commit,
            "clean": self.clean,
            "migrations": self.migrations,
            "sha256": self.sha256,
            "build": self.build,
            "size": self.size,
            "mtime_ns": self.mtime_ns,
            "inode": self.inode,
            "made_ms": made_ms,
        })
    }

    fn from_json(v: &Value) -> Option<Self> {
        Some(Self {
            commit: v["commit"].as_str().map(str::to_owned),
            clean: v["clean"].as_bool()?,
            migrations: v["migrations"]
                .as_object()?
                .iter()
                .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_owned())))
                .collect(),
            sha256: v["sha256"].as_str()?.to_owned(),
            build: v["build"].as_str().map(str::to_owned),
            size: v["size"].as_u64()?,
            mtime_ns: v["mtime_ns"].as_u64()?,
            inode: v["inode"].as_u64()?,
        })
    }
}

/// Which build a binary is, by the directory it stands in (wa_run.rs:1883 `build_kind`).
fn build_kind(binary: &Path) -> &'static str {
    match binary
        .parent()
        .and_then(Path::file_name)
        .and_then(|n| n.to_str())
    {
        Some("debug") => "debug",
        Some("release") => "release",
        _ => "other",
    }
}

/// (size, modification time in ns, inode) of a file: what ties a stamp to the file it was made for.
fn identity_of(meta: &std::fs::Metadata) -> (u64, u64, u64) {
    let mtime = u64::try_from(meta.mtime()).unwrap_or(0) * 1_000_000_000
        + u64::try_from(meta.mtime_nsec()).unwrap_or(0);
    (meta.len(), mtime, meta.ino())
}

/// Copies `source` to `tmp` (mode 0600) and returns its hash, which is the hash of both: the bytes
/// are hashed before and after the copy, so a binary that changed while it was copied (a build
/// running beside) is refused and `tmp` removed (wa_run.rs:1937 `copy_checked`).
pub(super) fn copy_checked(
    source: &Path,
    tmp: &Path,
    hash: &dyn Fn(&Path) -> Result<String>,
) -> Result<String> {
    let before = hash(source)?;
    // A copy a crashed start left is execute-only: it cannot be opened for writing again.
    let _ = std::fs::remove_file(tmp);
    {
        let mut from =
            std::fs::File::open(source).context("open the built app (run without --no-build)")?;
        let mut to = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(tmp)
            .context("create the copy of the app")?;
        std::io::copy(&mut from, &mut to).context("copy the app")?;
        to.sync_all()?;
    }
    if hash(tmp)? != before {
        let _ = std::fs::remove_file(tmp);
        bail!(
            "the built app changed while it was copied (a build running beside?): nothing was stamped. Start again."
        );
    }
    Ok(before)
}

/// Makes the profile's stamped copy of the app from `source` and its stamp. The copy is EXECUTE-ONLY
/// (mode 0100): no other process of the user can read `/proc/<pid>/environ` or `/mem` of the app
/// from its first instruction on. The old copy, which a running app may still be, is replaced by a
/// rename: it keeps running from its inode (wa_run.rs:1975 `stamp_copy`).
pub(super) fn stamp_copy(cleared: &Cleared, source: &Path, now: u64) -> Result<Stamp> {
    let dir = &cleared.dir;
    private_dir(&dir.bin_dir())?;
    let tmp = dir.bin_dir().join("kourier.new");
    let sha256 = copy_checked(source, &tmp, &sha256_of)?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o100))?;
    // No stamp stands while the copy is replaced: a crash in between leaves a copy `--no-build`
    // refuses, never a stamp for another file.
    let _ = std::fs::remove_file(dir.stamp_file());
    std::fs::rename(&tmp, dir.copy_file()).context("replace the stamped copy")?;
    let (size, mtime_ns, inode) = identity_of(&std::fs::metadata(dir.copy_file())?);
    let stamp = Stamp {
        commit: cleared.head.clone(),
        clean: cleared.head.is_some(),
        migrations: cleared.migrations.clone().unwrap_or_default(),
        sha256,
        build: Some(build_kind(source).to_owned()),
        size,
        mtime_ns,
        inode,
    };
    write_private(&dir.stamp_file(), &format!("{:#}\n", stamp.to_json(now)))?;
    Ok(stamp)
}

/// The copy as it stands, against its stamp: a stamp that can be read, a regular file of the very
/// size, time and inode it was stamped with, and mode 0100. The supervisor checks this at every
/// start; it does not say what the copy was built from ([`reuse_copy`] does; wa_run.rs:2005
/// `check_copy`).
pub(super) fn check_copy(dir: &ProfileDir) -> Result<Stamp> {
    let stamp = read_json(&dir.stamp_file())
        .ok()
        .flatten()
        .and_then(|v| Stamp::from_json(&v))
        .ok_or_else(|| {
            anyhow!(
                "this profile has no stamped copy of the app, or its stamp is unreadable: start without --no-build. Nothing was started."
            )
        })?;
    let meta = std::fs::symlink_metadata(dir.copy_file())
        .context("the stamped copy of the app is missing: start without --no-build")?;
    if !meta.file_type().is_file() {
        bail!(
            "the stamped copy of the app is not a regular file: start without --no-build. Nothing was started."
        );
    }
    let mode = meta.permissions().mode() & 0o7777;
    if mode != 0o100 {
        bail!(
            "the stamped copy of the app has mode {mode:o}, not 100 (execute only): its environment would be readable by every process of the user. Start without --no-build to make it again. Nothing was started."
        );
    }
    if identity_of(&meta) != (stamp.size, stamp.mtime_ns, stamp.inode) {
        bail!(
            "the stamped copy of the app is not the file that was stamped (its size, time or inode differ): start without --no-build. Nothing was started."
        );
    }
    Ok(stamp)
}

/// `--no-build`: the copy is reused only when it is intact ([`check_copy`]) and was built at the
/// commit this start runs at, from a clean tree (`expect` is main's HEAD; the rehearsal passes
/// `None` and its copy must have none either; wa_run.rs:2039 `reuse_copy`).
pub(super) fn reuse_copy(dir: &ProfileDir, expect: Option<&str>) -> Result<Stamp> {
    let stamp = check_copy(dir)?;
    if stamp.commit.as_deref() != expect || stamp.clean != expect.is_some() {
        bail!(
            "the stamped copy of the app was built at {}, and this start runs at {}{}: start without --no-build. Nothing was started.",
            stamp.commit.as_deref().map_or("no commit", short),
            expect.map_or("no commit", short),
            if stamp.clean {
                ""
            } else {
                " (the copy was not made from a clean tree)"
            }
        );
    }
    Ok(stamp)
}

// ---- The order and the app's command ----------------------------------------------------------------

/// The order `start` leaves for the supervisor (`launch.json`): the supervisor reads it ONCE (the
/// file is taken away), within [`LAUNCH_VALID_MS`], so a supervisor started by hand finds none and
/// starts nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Order {
    /// The control socket the supervisor binds (its name is [`socket_name`] of the profile).
    pub socket: PathBuf,
    /// The fake that plays Threema, offscreen and silent: the rehearsal's.
    pub rehearsal: bool,
    /// The start waits for the ID (`id`) so that a first link can follow.
    pub for_link: bool,
}

/// The JSON of an order (`bin` is the copy the supervisor will start: a check that the order and the
/// profile agree).
pub(super) fn order_json(dir: &ProfileDir, order: &Order, now: u64) -> Value {
    json!({
        "bin": dir.copy_file(),
        "issued_ms": now,
        "rehearsal": order.rehearsal,
        "for_link": order.for_link,
        "socket": order.socket,
    })
}

/// Reads the order `start` left and takes it away: one order starts the app once (wa_run.rs:5139
/// `take_launch`). It is taken away before it is judged (a refused order must not wait for a second
/// try), must be fresh, name this profile's copy, agree about the rehearsal, and name the socket
/// this profile's name gives, in a private directory of this user's.
pub(super) fn take_launch(dir: &ProfileDir, now: u64, uid: u32) -> Result<Order> {
    let file = dir.launch_file();
    let order = read_json(&file);
    let _ = std::fs::remove_file(&file);
    let order = order?.context(
        "no launch order: the supervisor is started by `cargo xtask threema-run start`, never by hand",
    )?;
    let issued = order["issued_ms"].as_u64().unwrap_or(0);
    if now.saturating_sub(issued) > LAUNCH_VALID_MS || issued > now + LAUNCH_VALID_MS {
        bail!("the launch order is stale: run `cargo xtask threema-run start` again");
    }
    if order["bin"] != json!(dir.copy_file()) {
        bail!("the launch order names another program than the profile's stamped copy");
    }
    let rehearsal = order["rehearsal"] == json!(true);
    if rehearsal != dir.rehearsal {
        bail!("the launch order and the profile disagree about the rehearsal");
    }
    let socket = PathBuf::from(
        order["socket"]
            .as_str()
            .context("the launch order names no control socket")?,
    );
    let parent = socket
        .parent()
        .filter(|p| p.is_absolute())
        .context("the launch order's control socket has no directory")?;
    if socket.file_name().and_then(|n| n.to_str()) != Some(socket_name(&dir.dir)?.as_str()) {
        bail!("the launch order names another control socket than this profile's");
    }
    // The same rule as the preflight's check 14: a private directory of this user's, short enough.
    let parent = parent.to_string_lossy().into_owned();
    check_socket_place(&|k| (k == "XDG_RUNTIME_DIR").then(|| parent.clone()), uid)?;
    Ok(Order {
        socket,
        rehearsal,
        for_link: order["for_link"] == json!(true),
    })
}

/// The inherited variables the app must not see: every `KOURIER_*` one (smoke.rs:838
/// `inherited_to_strip`). A leftover `KOURIER_DEV_REHEARSAL` would put the fake in a real run's
/// place, a `KOURIER_FAILPOINT` aborts the process, a leftover `KOURIER_DEV_EXPECT_NUMBER_TAIL` is
/// another network's gate.
fn inherited_to_strip(keys: &[OsString]) -> Vec<&OsString> {
    keys.iter()
        .filter(|key| key.to_string_lossy().starts_with("KOURIER_"))
        .collect()
}

/// What every start of the app (and its probe) shares: no inherited `KOURIER_*`, the profile and, in
/// a rehearsal, the data home the app derives the run profile's keeper root from (the profile's
/// parent's parent) and the switch that puts the fake in Threema's place. A real start leaves the
/// data home as the environment has it (the keeper root was derived from it).
fn base_env(cmd: &mut Command, dir: &ProfileDir, inherited: &[OsString], rehearsal: bool) {
    for key in inherited_to_strip(inherited) {
        cmd.env_remove(key);
    }
    cmd.env("KOURIER_HOME", &dir.dir);
    if rehearsal {
        if let Some(data_home) = dir.keeper().parent() {
            cmd.env("XDG_DATA_HOME", data_home);
        }
        cmd.env(REHEARSAL_ENV, REHEARSAL_WORD);
    }
}

/// No window: the app draws on Qt's offscreen platform.
fn offscreen(cmd: &mut Command) {
    cmd.env("QT_QPA_PLATFORM", "offscreen")
        .env("QT_QUICK_BACKEND", "software")
        .env("QT_SCALE_FACTOR", "1")
        .env("LANG", "en_US.UTF-8")
        .env("LANGUAGE", "en");
}

/// No sound: a program that makes one finds no audio server.
fn no_audio(cmd: &mut Command) {
    cmd.env("PULSE_SERVER", NO_AUDIO_SERVER)
        .env("PIPEWIRE_REMOTE", NO_PIPEWIRE)
        .env_remove("PULSE_RUNTIME_PATH");
}

/// The command that starts the app (wa_run.rs:2098 `app_command`, tg_run/connect.rs:173): the
/// stamped copy itself, never `kcap`, `systemd-run`, `env`, `timeout` or a shell, and the
/// environment below, which a test pins whole. The real run shows the window (he must see the QR
/// code); the rehearsal runs offscreen with the fake that plays Threema.
pub(super) fn app_command(
    bin: &Path,
    dir: &ProfileDir,
    order: &Order,
    inherited: &[OsString],
) -> Command {
    let mut cmd = Command::new(bin);
    cmd.arg("--dev-control");
    base_env(&mut cmd, dir, inherited, order.rehearsal);
    cmd.env("KOURIER_DEV_KEYRING", "passphrase")
        // A crash must not leave the account's keys in a core dump (critic R-H1).
        .env("KOURIER_DEV_NO_DUMP", "1")
        // The QR code is shown in the window, never in an event or a log.
        .env("KOURIER_DEV_QR_URL", "0")
        // No notifications (names and texts on the desktop), no tray, no bus name (critic R-H1).
        .env("KOURIER_DESKTOP", "0")
        .env("KOURIER_LOG", APP_LOG_FILTER)
        // Qt's own warnings go to the stderr the supervisor reads, not to the journal.
        .env("QT_FORCE_STDERR_LOGGING", "1");
    no_audio(&mut cmd);
    if order.rehearsal {
        cmd.arg("--size=1200x800");
        offscreen(&mut cmd);
    }
    cmd
}

/// What the supervisor will start, for the person to read: the program's file name, its arguments
/// and the NAMES of the variables set for it, never a value and never a path.
pub(super) fn describe_launch(cmd: &Command) -> String {
    let mut names: Vec<String> = cmd
        .get_envs()
        .filter(|(_, value)| value.is_some())
        .map(|(key, _)| key.to_string_lossy().into_owned())
        .collect();
    names.sort();
    let args: Vec<String> = cmd
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    format!(
        "{} {} (variables set by the tool: {})",
        Path::new(cmd.get_program())
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned()),
        args.join(" "),
        names.join(" ")
    )
}

// ---- The probe --------------------------------------------------------------------------------------

/// The command of the probe: the stamped app asked which networks it would register
/// (`--print-networks`, which opens no window, database or keyring and contacts nothing) over this
/// profile. It carries no `KOURIER_*` but the profile and, in a rehearsal, the fake's switch; it
/// runs offscreen and silent whatever the real run does.
pub(super) fn probe_command(
    bin: &Path,
    dir: &ProfileDir,
    rehearsal: bool,
    inherited: &[OsString],
) -> Command {
    let mut cmd = Command::new(bin);
    cmd.arg("--print-networks");
    base_env(&mut cmd, dir, inherited, rehearsal);
    offscreen(&mut cmd);
    no_audio(&mut cmd);
    cmd
}

/// Keeps what a probe's line may carry: kinds and the separators of the list.
fn kinds_only(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == ',')
        .collect()
}

/// The probe's answer: `networks registered=<kinds> off=<kinds> on=<kinds>` names Threema in
/// `registered=` (and in `on=`: the profile's `networks-on` was read). The refusal says what the
/// line said (kinds only).
pub(super) fn judge_probe(stdout: &str) -> Result<()> {
    let line = stdout
        .lines()
        .find(|l| l.starts_with("networks "))
        .ok_or_else(|| {
            anyhow!(
                "the stamped app printed no `networks` line: it would not start from this profile. Nothing was started."
            )
        })?;
    let list = |key: &str| {
        line.split_whitespace()
            .find_map(|w| w.strip_prefix(key))
            .map(kinds_only)
            .unwrap_or_default()
    };
    let (registered, on) = (list("registered="), list("on="));
    let names = |l: &str| l.split(',').any(|k| k == NETWORK_NAME);
    if !names(&on) {
        bail!(
            "the switch did not take: the stamped app read `networks-on` as `{on}` (it must name {NETWORK_NAME}). Nothing was started."
        );
    }
    if !names(&registered) {
        bail!(
            "the stamped app would not register {NETWORK_NAME} (registered=`{registered}`, off=`{}`, on=`{on}`): a build without the backend or a release build. Nothing was started.",
            list("off=")
        );
    }
    Ok(())
}

/// A clock a wait polls: the real one counts elapsed milliseconds, a test's moves only when a wait
/// finds nothing, so no test depends on the wall time (tg_run `Clock`, `StepClock`).
pub(super) trait WaitClock {
    fn now_ms(&self) -> u64;
    /// Called after every poll that found nothing.
    fn idle(&self);
}

/// The wall clock.
pub(super) struct RealClock(Instant);

impl RealClock {
    pub fn new() -> Self {
        Self(Instant::now())
    }
}

impl WaitClock for RealClock {
    fn now_ms(&self) -> u64 {
        u64::try_from(self.0.elapsed().as_millis()).unwrap_or(u64::MAX)
    }
    fn idle(&self) {}
}

/// How long a bounded command is let run, looked at how often, on which clock.
pub(super) struct Wait<'a> {
    pub bound: Duration,
    pub poll: Duration,
    pub clock: &'a dyn WaitClock,
}

/// What a bounded command did.
pub(super) struct Bounded {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr_lines: usize,
}

/// Runs `cmd` to its end for at most `wait.bound` on `wait.clock` (the process is killed at the
/// bound), looked at every `wait.poll`, its output read by threads of their own so a full pipe
/// cannot stop it (tg_run/stage.rs:630 `run_bounded`).
pub(super) fn run_bounded(mut cmd: Command, wait: &Wait<'_>) -> Result<Bounded> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().context("start the stamped app")?;
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
            bail!("the stamped app did not finish its probe in time and was killed");
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
    })
}

/// Runs the probe and judges it. A failed probe says its exit status and how many lines it wrote to
/// stderr; the lines are not kept (the redaction that keeps an app's stderr is the supervisor's).
pub(super) fn probe(
    bin: &Path,
    dir: &ProfileDir,
    rehearsal: bool,
    inherited: &[OsString],
    wait: &Wait<'_>,
) -> Result<()> {
    let result = run_bounded(probe_command(bin, dir, rehearsal, inherited), wait)?;
    if result.code != Some(0) {
        bail!(
            "the stamped app's probe failed (exit status {}, {} line(s) on stderr, not kept). Nothing was started.",
            result.code.map_or("none".to_owned(), |c| c.to_string()),
            result.stderr_lines
        );
    }
    judge_probe(&result.stdout)
}

// ---- The control socket -----------------------------------------------------------------------------

/// The socket's file name: `kthr-<16 hex>.sock`, the hex the first 8 bytes of the SHA-256 of the
/// profile's path (a path is no secret). The name has the length [`SOCKET_NAME_LEN`] less its slash.
pub(super) fn socket_name(profile: &Path) -> Result<String> {
    let sum = sha256_bytes(profile.as_os_str().as_bytes())?;
    Ok(format!("kthr-{}.sock", &sum[..16]))
}

/// The control socket of `profile` in `socket_dir` (`XDG_RUNTIME_DIR`, or a rehearsal's directory):
/// its length depends on the directory only, never on the checkout or the profile (critic C7).
pub(super) fn socket_path(socket_dir: &Path, profile: &Path) -> Result<PathBuf> {
    let socket = socket_dir.join(socket_name(profile)?);
    if socket.as_os_str().len() > SOCKET_PATH_MAX
        || socket_dir.as_os_str().len() + SOCKET_NAME_LEN > SOCKET_PATH_MAX
    {
        bail!(
            "the control socket's directory is {} bytes long: the socket's name would not fit a socket path ({SOCKET_PATH_MAX} bytes). Nothing was started.",
            socket_dir.as_os_str().len()
        );
    }
    Ok(socket)
}

/// A rehearsal's socket directory: `<base>/kthr-<uid>-<secs>`, `<secs>` the first component of the
/// place below `<target>/thr/` (0700, this user's: the same check as the real place's).
pub(super) fn rehearsal_socket_dir(
    dir: &ProfileDir,
    target: &Path,
    base: &Path,
    uid: u32,
) -> Result<PathBuf> {
    let rel = dir
        .dir
        .strip_prefix(target.join(REHEARSAL_DIR))
        .map_err(|_| anyhow!("the rehearsal's place is not below its directory"))?;
    let stamp = rel
        .components()
        .next()
        .and_then(|c| c.as_os_str().to_str())
        .filter(|s| {
            (1..=24).contains(&s.len())
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        })
        .ok_or_else(|| {
            anyhow!("the rehearsal's place has no short name of its own to key its socket on")
        })?;
    let place = base.join(format!("kthr-{uid}-{stamp}"));
    private_dir(&place)?;
    let text = place.to_string_lossy().into_owned();
    check_socket_place(&|k| (k == "XDG_RUNTIME_DIR").then(|| text.clone()), uid)
}

/// Binds the control socket at `socket` (mode 0600, in a private directory). A socket that answers
/// is another supervisor's: refused. A stale one is replaced (wa_run.rs:5113 `listen_at`).
pub(super) fn listen_at(socket: &Path) -> Result<UnixListener> {
    if socket.as_os_str().len() > SOCKET_PATH_MAX {
        bail!(
            "the control socket's path is too long for a Unix socket ({} of {SOCKET_PATH_MAX} bytes)",
            socket.as_os_str().len()
        );
    }
    if UnixStream::connect(socket).is_ok() {
        bail!("a supervisor already listens on this profile");
    }
    let _ = std::fs::remove_file(socket);
    let listener = UnixListener::bind(socket).context("bind the control socket")?;
    std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

/// One `info` request over the control socket: one JSON line in, one JSON line out; the answer is
/// `{"ok": true, ...}` with the fields at its top level (`pid`, `ready`, `id`, `halted`,
/// `self_dumpable`, `app_pid`: the supervisor's contract with `start`, wa_run.rs:2543 `info`).
pub(super) fn info_at(socket: &Path, timeout: Duration) -> Result<Value> {
    let mut stream = UnixStream::connect(socket).context("connect to the control socket")?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    writeln!(stream, "{}", json!({"op": "info"}))?;
    let mut line = String::new();
    BufReader::new(stream)
        .read_line(&mut line)
        .context("the supervisor did not answer")?;
    let answer: Value = serde_json::from_str(&line).context("the supervisor's answer")?;
    if answer["ok"] != json!(true) {
        bail!("the supervisor refused the request");
    }
    Ok(answer)
}

/// The control socket a client of a RUNNING supervisor uses: the one `supervisor.json` records
/// (`"socket"`: the supervisor writes it at boot), else the one `XDG_RUNTIME_DIR` and the profile
/// give.
pub(super) fn socket_for_client(
    dir: &ProfileDir,
    vars: &dyn Fn(&str) -> Option<String>,
) -> Result<PathBuf> {
    if let Some(recorded) =
        read_json(&dir.supervisor_file())?.and_then(|v| v["socket"].as_str().map(PathBuf::from))
    {
        return Ok(recorded);
    }
    let run = vars("XDG_RUNTIME_DIR")
        .filter(|d| !d.is_empty())
        .context("XDG_RUNTIME_DIR is not set and no supervisor recorded its socket")?;
    socket_path(Path::new(&run), &dir.dir)
}

// ---- The profile's lock -----------------------------------------------------------------------------

/// The profile's lock, taken for as long as the supervisor lives: one supervisor per profile
/// (wa_run.rs:5188 `take_profile_lock`). A lock that looks held is asked for again for a moment.
/// The supervisor lets it go only after the app's process is gone: `start`'s guard reads a free lock
/// as "nobody runs".
pub(super) fn take_profile_lock(dir: &ProfileDir) -> Result<std::fs::File> {
    private_dir(&dir.tool())?;
    let file = open_lock_file(&dir.lock_file()).context("open the profile's lock file")?;
    if !try_lock_for(&file, LOCK_SETTLE)? {
        bail!("another threema-run supervisor holds this profile's lock");
    }
    Ok(file)
}

/// Whether nobody holds the profile's lock: no supervisor lives on it (wa_run.rs:5640
/// `lock_is_free`, settled: a lock only a child about to `exec` carries is free).
pub(super) fn lock_is_free(dir: &ProfileDir) -> Result<bool> {
    let file = open_lock_file(&dir.lock_file())?;
    try_lock_for(&file, LOCK_SETTLE)
}

// ---- The switch and its guard -----------------------------------------------------------------------

/// Writes the switch ON (`on_text` is the preflight's plan) into the profile's `kourier.toml`.
fn switch_on(dir: &ProfileDir, on_text: &str) -> Result<()> {
    private_dir(&dir.app_home())?;
    private_dir(
        dir.config_file()
            .parent()
            .ok_or_else(|| anyhow!("the config directory"))?,
    )?;
    write_private(&dir.config_file(), on_text)
}

/// Writes the switch OFF: the profile's `kourier.toml` as it is NOW with `networks-on = []` (never
/// the text the start planned from: the app may have rewritten the file meanwhile). `true` when the
/// file changed; a missing file is off already. Every end of a run calls it, and only after the
/// app's process is gone (critic M3 and C3).
pub(super) fn switch_off(dir: &ProfileDir) -> Result<bool> {
    let text = match std::fs::read_to_string(dir.config_file()) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e).context("read the profile's kourier.toml"),
    };
    let off = switch_text(Some(&text), false)?;
    if off == text {
        return Ok(false);
    }
    write_private(&dir.config_file(), &off)?;
    Ok(true)
}

/// Whether the profile's switch names the network right now (a test reads it; the supervisor's
/// `state` says it too).
pub(super) fn switch_is_on(dir: &ProfileDir) -> bool {
    std::fs::read_to_string(dir.config_file()).is_ok_and(|text| {
        text.lines().any(|l| {
            l.trim_start()
                .strip_prefix("networks-on")
                .is_some_and(|r| r.trim_start().starts_with('=') && r.contains(NETWORK_NAME))
        })
    })
}

/// Holds the switch from its write on (critic M3): unless it is [`disarm`](Self::disarm)ed, a
/// refusal or failure before the supervisor answered puts it back off. Once a supervisor was
/// started the guard does so only when that supervisor is gone (`gone`), else it says the switch
/// stays on until `stop`.
pub(super) struct SwitchGuard {
    dir: ProfileDir,
    armed: bool,
    gone: Option<Box<dyn Fn() -> bool>>,
}

impl SwitchGuard {
    /// Writes the switch ON and holds it. A write that fails is put back by the guard's own drop.
    pub fn arm(dir: &ProfileDir, on_text: &str) -> Result<Self> {
        let guard = Self {
            dir: dir.clone(),
            armed: true,
            gone: None,
        };
        switch_on(dir, on_text)?;
        Ok(guard)
    }

    /// The supervisor was started: from now on the switch is put back only when `gone` says nobody
    /// runs.
    pub fn supervisor_started(&mut self, gone: impl Fn() -> bool + 'static) {
        self.gone = Some(Box::new(gone));
    }

    /// The supervisor answered: it owns the switch now.
    pub fn disarm(mut self) {
        self.armed = false;
    }

    /// Puts the switch back off when that is safe; the words say what was done (never a path).
    pub fn put_back(&mut self) -> Vec<String> {
        self.armed = false;
        if let Some(gone) = &self.gone
            && !gone()
        {
            return vec![SWITCH_LEFT_ON.to_owned()];
        }
        match switch_off(&self.dir) {
            Ok(true) => vec![format!(
                "{NETWORK_NAME} is switched off in the profile again: the start did not finish."
            )],
            Ok(false) => Vec::new(),
            Err(_) => vec![
                "warning: the switch could not be put back off (kourier.toml cannot be read or written): run `threema-run stop` before anything else."
                    .to_owned(),
            ],
        }
    }
}

impl Drop for SwitchGuard {
    fn drop(&mut self) {
        if self.armed {
            for line in self.put_back() {
                eprintln!("{line}");
            }
        }
    }
}

// ---- A signal during the start (LOW-4) --------------------------------------------------------------

/// Set by the handlers of [`SignalWatch`].
static START_SIGNALLED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_start_signal(_signal: libc::c_int) {
    START_SIGNALLED.store(true, Ordering::SeqCst);
}

/// The signals that would end `start` where it stands.
const WATCHED_SIGNALS: [libc::c_int; 3] = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP];

/// From the switch's write until the supervisor answered, a SIGINT, SIGTERM or SIGHUP does not kill
/// `start` where it stands (that would leave Threema switched ON in the profile with no supervisor,
/// the gap of LOW-4: the lead's shell call ending while the probe runs, a Ctrl-C): the handlers only
/// note it, the start looks at [`caught`](Self::caught) between its steps and ends with an error, and
/// the [`SwitchGuard`]'s drop puts the switch back. The earlier dispositions come back when this is
/// dropped. A child never inherits the handlers (an exec resets them to the default).
pub(super) struct SignalWatch {
    old: Vec<(libc::c_int, libc::sigaction)>,
}

impl SignalWatch {
    pub fn arm() -> Self {
        START_SIGNALLED.store(false, Ordering::SeqCst);
        let mut old = Vec::new();
        for signal in WATCHED_SIGNALS {
            // SAFETY: both structs are plain data, zero is a valid state for them; `sigaction`
            // gets pointers to them; the handler only stores into an atomic, which is
            // async-signal-safe. No `SA_RESTART`: a blocking wait of this process is not resumed
            // silently for good, and the standard library retries its own waits on `EINTR`.
            unsafe {
                let mut new: libc::sigaction = std::mem::zeroed();
                new.sa_sigaction = on_start_signal as *const () as libc::sighandler_t;
                libc::sigemptyset(&raw mut new.sa_mask);
                let mut previous: libc::sigaction = std::mem::zeroed();
                if libc::sigaction(signal, &raw const new, &raw mut previous) == 0 {
                    old.push((signal, previous));
                }
            }
        }
        Self { old }
    }

    /// Whether a watched signal came since [`arm`](Self::arm).
    pub fn caught(&self) -> bool {
        START_SIGNALLED.load(Ordering::SeqCst)
    }

    /// An error that ends the start when a signal came: the words say what is left.
    fn check(&self, supervisor_started: bool) -> Result<()> {
        if !self.caught() {
            return Ok(());
        }
        if supervisor_started {
            bail!(
                "a signal came while the start waited for the supervisor: the start ends here. The supervisor was started and owns the switch: `threema-run state` says whether it runs, `threema-run stop` ends it"
            )
        }
        bail!(
            "a signal came before the supervisor was started: the start ends here and Threema is switched off in the profile again. Nothing is running"
        )
    }
}

impl Drop for SignalWatch {
    fn drop(&mut self) {
        for (signal, previous) in &self.old {
            // SAFETY: puts back the disposition `sigaction` returned for this very signal.
            unsafe {
                libc::sigaction(*signal, previous, std::ptr::null_mut());
            }
        }
    }
}

// ---- The open question ------------------------------------------------------------------------------

/// Opens the question of a failed link in the KEEPER ROOT (critic M4): the kind word and the time,
/// no other value, 0600, fsynced. A `wipe` and a link start refuse while it stands; `answer` (T4)
/// removes it.
pub(super) fn open_question(dir: &ProfileDir, now: u64) -> Result<()> {
    open_question_noting(dir, now, None)
}

/// [`open_question`], with what Kourier's own drop of the device came to when the run saw RP-b's
/// outcome line (critic M2: `removed` or `not-reached`; the word only), so `answer` can say it
/// before the question. The question stays either way: it is the backstop.
pub(super) fn open_question_noting(dir: &ProfileDir, now: u64, drop: Option<&str>) -> Result<()> {
    let mut question = json!({"kind": QUESTION_KIND, "at_ms": now});
    if let Some(drop) = drop {
        question["drop"] = json!(drop);
    }
    write_private(&dir.open_question_file(), &format!("{question:#}\n"))
}

// ---- start ------------------------------------------------------------------------------------------

/// The profile's directories (0700); the state file only when the profile has none: an existing
/// state is saved once, late, with everything this start changes (a start that refuses leaves it
/// as it was).
fn make_places(dir: &ProfileDir, state: &State) -> Result<()> {
    check_canonical(&dir.dir)?;
    private_dir(dir.keeper())?;
    private_dir(&dir.dir)?;
    private_dir(&dir.tool())?;
    if std::fs::symlink_metadata(dir.state_file()).is_err() {
        state.save(dir)?;
    }
    Ok(())
}

/// How long `start` waits for what it started (a test shortens them).
pub(super) struct UpTiming {
    /// For the supervisor's socket to answer.
    pub come_up: Duration,
    /// After this long a supervisor with no socket and a free lock is taken for gone: the first
    /// moments it has neither.
    pub gone_after: Duration,
    pub poll: Duration,
}

impl UpTiming {
    pub const REAL: Self = Self {
        come_up: Duration::from_secs(120),
        gone_after: Duration::from_secs(3),
        poll: Duration::from_millis(200),
    };
}

/// What `start` found out, for the person to read.
#[derive(Debug)]
pub(super) struct Up {
    pub lines: Vec<String>,
    pub ok: bool,
}

/// The parts of `start` a test replaces.
pub(super) struct Seams<'a> {
    /// The build (`cargo build -p kourier`); a rehearsal never builds (it ran outside the
    /// namespace).
    pub build: &'a dyn Fn() -> Result<()>,
    /// The binary the build makes.
    pub binary: &'a Path,
    /// The names of the environment's variables (the app must see no `KOURIER_*` of them).
    pub inherited: &'a [OsString],
    /// Runs the probe on the stamped copy (`probe` over the real clock).
    pub probe: &'a dyn Fn(&ProfileDir, &Path, bool) -> Result<()>,
    /// Starts the supervisor, detached.
    pub spawn: &'a dyn Fn(&ProfileDir) -> Result<()>,
    /// Where a rehearsal's socket directory is made (`/dev/shm`).
    pub shm_base: &'a Path,
    pub up: &'a UpTiming,
    /// The line the user pastes into a terminal of his own to type the ID.
    pub id_line: &'a str,
}

/// The preflight, then everything up to a supervisor that answers (the module's order). Returns the
/// lines to print; an error leaves the switch off (or says it stays on for a live supervisor).
pub(super) fn start_in(a: &Args, ctx: &Ctx<'_>, seams: &Seams<'_>) -> Result<Up> {
    let mut cleared = preflight(a, ctx)?;
    let dir = cleared.dir.clone();
    let mut lines: Vec<String> = cleared
        .warnings
        .iter()
        .map(|w| format!("warning: {w}"))
        .collect();
    lines.push(format!("profile:    {}", dir.label()));
    if cleared.encrypted == Some(true) {
        lines.push("disk:       encrypted (dm-crypt)".to_owned());
    }
    // 1. places and the socket's directory.
    make_places(&dir, &cleared.state)?;
    let socket_dir = match (&cleared.socket_dir, dir.rehearsal) {
        (Some(socket_dir), _) => socket_dir.clone(),
        (None, true) => rehearsal_socket_dir(&dir, ctx.target, seams.shm_base, ctx.uid)?,
        (None, false) => bail!("internal: a real start has no place for the control socket"),
    };
    let socket = socket_path(&socket_dir, &dir.dir)?;
    // 2. a leftover list of test IDs (the preflight read it; the start removes it).
    if cleared.leftover_list {
        lines.extend(remove_leftover_list(&dir)?);
    }
    // 3. the stamped copy.
    let stamp = if a.no_build {
        reuse_copy(&dir, cleared.head.as_deref())?
    } else {
        (seams.build)()?;
        // The stamp says "main's HEAD, clean": the tree must still be what the preflight saw.
        if let Some(head) = &cleared.head
            && check_source(ctx.git)? != *head
        {
            bail!("main moved while the app was built: nothing was stamped. Start again.");
        }
        stamp_copy(&cleared, seams.binary, (ctx.now_ms)())?;
        check_copy(&dir)?
    };
    if let Some(head) = &cleared.head {
        lines.push(format!(
            "source:     main at {}, clean; {} migration files tracked",
            short(head),
            stamp.migrations.len()
        ));
    }
    lines.push(format!(
        "app:        stamped copy at {}, execute-only, sha256 {}...",
        stamp.commit.as_deref().map_or("no commit", short),
        &stamp.sha256[..12]
    ));
    // 4. a login that stood at `linking`: the question is open before anything goes on.
    if cleared.link_stood {
        open_question(&dir, (ctx.now_ms)())?;
        cleared.state.set("link_at", Value::Null);
        lines.push(
            "QUESTION:   a login of this profile stood at linking and its end was not seen: a device may be registered on the TEST phone. The question is open in the keeper root: no link, and no wipe, until it is answered (`threema-run answer`, in the user's own terminal)."
                .to_owned(),
        );
    }
    // 5. the switch, held. 6. the probe. A signal from here on ends the start in order (LOW-4): the
    // watch is armed BEFORE the switch is written, so none falls in the gap.
    let watch = SignalWatch::arm();
    let mut guard = SwitchGuard::arm(&dir, &cleared.switch.on_text)?;
    (seams.probe)(&dir, &dir.copy_file(), dir.rehearsal)?;
    watch.check(false)?;
    lines.push(format!(
        "probe:      the app registers {NETWORK_NAME} from this profile (the switch is ON for this start)"
    ));
    // 7. the state, the order, the supervisor.
    let starts = cleared.state.num("starts").unwrap_or(0) + 1;
    // The TEST phone of the run: the flag wins over the kept value (D1).
    if a.no_test_phone && a.test_phone.is_some() {
        bail!("--test-phone and --no-test-phone together: nothing was started");
    }
    if a.no_test_phone {
        cleared.state.set("test_phone", Value::Null);
        lines.push("test phone: forgotten for this profile".to_owned());
    }
    if let Some(serial) = &a.test_phone {
        super::tphone::check_serial(serial)?;
        cleared.state.set("test_phone", json!(serial));
    }
    if let Some(serial) = cleared.state.test_phone() {
        lines.push(format!(
            "test phone: {serial} (kept in the profile; the steps take it from the supervisor)"
        ));
    }
    cleared.state.set("starts", json!(starts));
    cleared.state.set("build_commit", json!(cleared.head));
    let order = Order {
        socket: socket.clone(),
        rehearsal: dir.rehearsal,
        for_link: a.for_link,
    };
    // The order first, then the state: an order that cannot be written leaves the state as it was
    // (a halt `--after-halt` moved is not spent by a start that never began), and an order whose
    // state cannot be saved goes again.
    write_private(
        &dir.launch_file(),
        &format!("{:#}\n", order_json(&dir, &order, (ctx.now_ms)())),
    )?;
    if let Err(e) = cleared.state.save(&dir) {
        let _ = std::fs::remove_file(dir.launch_file());
        return Err(e);
    }
    lines.push(format!("start:      number {starts} of this profile"));
    let cmd = app_command(&dir.copy_file(), &dir, &order, seams.inherited);
    lines.push(format!("will start: {}", describe_launch(&cmd)));
    // The last look before the supervisor exists: the order is taken back with the start.
    if let Err(e) = watch.check(false) {
        let _ = std::fs::remove_file(dir.launch_file());
        return Err(e);
    }
    (seams.spawn)(&dir)?;
    let watched = dir.clone();
    guard.supervisor_started(move || lock_is_free(&watched).unwrap_or(false));
    let up = bring_up(&dir, &socket, a.for_link, seams.id_line, seams.up, &watch)?;
    guard.disarm();
    lines.extend(up.lines);
    Ok(Up { lines, ok: up.ok })
}

/// Waits for what the start is for: the supervisor answers `info` and is ready (the app's dev
/// channel is up), or, in a start for a link, waits for the ID. An error says the supervisor halted,
/// was dumpable, or never came up (wa_run.rs:5668 `bring_up`).
fn bring_up(
    dir: &ProfileDir,
    socket: &Path,
    for_link: bool,
    id_line: &str,
    timing: &UpTiming,
    watch: &SignalWatch,
) -> Result<Up> {
    let spawned = Instant::now();
    let info = loop {
        watch.check(true)?;
        if let Ok(info) = info_at(socket, Duration::from_secs(5)) {
            if let Some(kind) = info["halted"].as_str() {
                bail!(
                    "the run halted as it started ({kind}). Nothing is retried, and a new start needs --after-halt and the user's word; `threema-run state` says more."
                );
            }
            if info["self_dumpable"] == json!(true) {
                bail!(
                    "the supervisor is dumpable: a crash would leave the account's keys in a core file. Run `threema-run stop`."
                );
            }
            if (for_link && info["id"] == json!(INFO_ID_WAITING)) || info["ready"] == json!(true) {
                break info;
            }
        }
        // The supervisor gave up (it holds the lock for as long as it lives).
        let gone = spawned.elapsed() > timing.gone_after
            && lock_is_free(dir).unwrap_or(false)
            && !socket.exists();
        if spawned.elapsed() >= timing.come_up || gone {
            bail!(
                "the supervisor did not come up (read the tool's supervisor.log in the profile's threema-run directory)."
            );
        }
        std::thread::sleep(timing.poll);
    };
    let mut lines = vec![format!("supervisor: up (pid {})", info["pid"])];
    if for_link && info["id"] == json!(INFO_ID_WAITING) {
        lines.push(
            "id:         the user types the TEST phone's ID himself (the ID the TEST phone shows under My ID, never his personal one), in a terminal of his own, twice, nothing shown; Claude never sees it:"
                .to_owned(),
        );
        lines.push(format!("              {id_line}"));
        lines.push(
            "next:       after \"taken (8 characters)\" the app starts: cargo xtask threema-run step link"
                .to_owned(),
        );
    } else {
        lines.push(format!(
            "app:        dev channel up (pid {}); a crash leaves no core dump",
            info["app_pid"]
        ));
        lines.push("next:       cargo xtask threema-run step reconnect".to_owned());
    }
    Ok(Up { lines, ok: true })
}

// ---- The real start ---------------------------------------------------------------------------------

/// The command that starts the supervisor: this very program again, `threema-run __supervise`, and
/// the flags that name the profile. It holds no ID and no digest: nothing it is given ever will.
fn supervisor_command(a: &Args) -> Result<Command> {
    let mut cmd = Command::new(std::env::current_exe().context("the xtask binary")?);
    cmd.args(["threema-run", "__supervise"])
        .args(a.place.flags());
    Ok(cmd)
}

/// Starts `cmd` in a session of its own, its output in `log`: the supervisor outlives the shell call
/// that started it, and a Ctrl-C in that shell does not reach it (wa_run.rs:5563 `detached`).
pub(super) fn detached(mut cmd: Command, log: &Path) -> Result<std::process::Child> {
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(log)
        .context("open the supervisor's log")?;
    cmd.stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    // SAFETY: `setsid` is async-signal-safe and only detaches the child from this session; nothing
    // else runs between fork and exec.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    cmd.spawn().context("start the supervisor")
}

pub(super) fn real_build() -> Result<()> {
    crate::run(
        "build kourier (debug)",
        crate::cargo().args(["build", "-p", "kourier"]),
    )
}

/// `threema-run start`: the preflight over this machine, then [`start_in`] with the real build, the
/// real probe and the detached supervisor. Prints what it did, counts and words only.
pub(super) fn start(a: &Args) -> Result<i32> {
    with_real_ctx(|ctx| {
        let inherited: Vec<OsString> = std::env::vars_os().map(|(key, _)| key).collect();
        let binary = ctx.target.join("debug").join("kourier");
        let rehearsal = a.place.rehearsal_place.is_some();
        // A rehearsal never builds: it runs in `unshare -rn`, where cargo would rebuild part of the
        // tree; `rehearse` built the app before the first command.
        let build = || if rehearsal { Ok(()) } else { real_build() };
        let clock = RealClock::new();
        let probe_it = |dir: &ProfileDir, bin: &Path, rehearsal: bool| {
            probe(
                bin,
                dir,
                rehearsal,
                &inherited,
                &Wait {
                    bound: Duration::from_secs(60),
                    poll: Duration::from_millis(50),
                    clock: &clock,
                },
            )
        };
        let spawn = |dir: &ProfileDir| {
            detached(supervisor_command(a)?, &dir.supervisor_log())?;
            Ok(())
        };
        let id_line = format!(
            "{} threema-run id {}",
            ctx.target.join("debug").join("xtask").display(),
            a.place.flags().join(" ")
        );
        let seams = Seams {
            build: &build,
            binary: &binary,
            inherited: &inherited,
            probe: &probe_it,
            spawn: &spawn,
            shm_base: Path::new(REHEARSAL_SOCKET_BASE),
            up: &UpTiming::REAL,
            id_line: &id_line,
        };
        let up = start_in(a, ctx, &seams)?;
        for line in &up.lines {
            println!("{line}");
        }
        Ok(i32::from(!up.ok))
    })
}
