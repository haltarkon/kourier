//! The kept profile of `tg-run`: where it lives, the places it must not live in, the disk it is on,
//! the private files, the tool's state file and `wipe`.
//!
//! The keeper root is `${XDG_DATA_HOME:-~/.local/share}/kourier-tg-run` (0700); it holds the
//! developer's API file (HIS file, written by himself; the tool never reads, copies or deletes it)
//! and one directory per profile, which is the app's `KOURIER_HOME`. Everything of the tool's own is
//! below `<profile>/tg-run/`.
//!
//! Every helper here is a COPY of `wa_run.rs` (each names its source as `(wa_run.rs:<line> <fn>)`,
//! the precedent of that file's own copies of `smoke.rs`): the run tools are not shared before the
//! extraction `.claude/rules/dev-runs.md` lists, so a change to one cannot move another.

use std::fs::OpenOptions;
use std::io::Write as _;
use std::os::unix::fs::{
    DirBuilderExt as _, MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _,
};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

use super::{API_FILE, KEEPER_DIR, REHEARSAL_DIR, TOOL_DIR};

/// Creates `dir` with mode 0700 (and missing parents) and tightens an existing one (wa_run.rs:358
/// `private_dir`).
pub(super) fn private_dir(dir: &Path) -> Result<()> {
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

/// Writes `text` to `path` (mode 0600) through a temporary file and a rename (wa_run.rs:373
/// `write_private`).
pub(super) fn write_private(path: &Path, text: &str) -> Result<()> {
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

/// A JSON file; `None` when it is not there (wa_run.rs:408 `read_json`).
pub(super) fn read_json(path: &Path) -> Result<Option<Value>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(
            serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))?,
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

/// A profile name (wa_run.rs:419 `valid_profile_name`). Two additions: a name holding seven or more
/// digits in a row is refused, because the profile's NAME is the one thing of the profile the
/// tool prints, and a phone number given as a name would reach a report; and the API file's own name
/// is refused (H-6), because a profile of that name would BE the user's API file.
pub(super) fn valid_profile_name(name: &str) -> bool {
    (1..=32).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        && !holds_a_number(name)
        && name != API_FILE
}

/// Whether `text` has a run of 7 or more ASCII digits (the length of the shortest phone number).
fn holds_a_number(text: &str) -> bool {
    let mut run = 0;
    for b in text.bytes() {
        if b.is_ascii_digit() {
            run += 1;
            if run >= 7 {
                return true;
            }
        } else {
            run = 0;
        }
    }
    false
}

/// The directory of the kept profiles: `${XDG_DATA_HOME:-~/.local/share}/kourier-tg-run`
/// (wa_run.rs:429 `keeper_root`, with this tool's directory; the app's `keeper_root` is its twin and
/// the X16 test pins the names). It takes the environment, so a test puts the whole thing below a
/// scratch directory.
pub(super) fn keeper_root(env: &dyn Fn(&str) -> Option<String>) -> Result<PathBuf> {
    let abs = |k: &str| env(k).map(PathBuf::from).filter(|p| p.is_absolute());
    let data = abs("XDG_DATA_HOME")
        .or_else(|| abs("HOME").map(|h| h.join(".local/share")))
        .ok_or_else(|| anyhow!("neither XDG_DATA_HOME nor HOME is set"))?;
    Ok(data.join(KEEPER_DIR))
}

/// What protects the directory at rest: `Some(..)` when the file system below `dir` is on a
/// dm-crypt device (LUKS), looked up from its device number in sysfs (wa_run.rs:440
/// `disk_encryption`). The answer says "dm-crypt" and no more: the device's name is not printed.
pub(super) fn disk_encryption(dir: &Path) -> Option<String> {
    let existing = dir.ancestors().find(|p| p.exists())?;
    let dev = std::fs::metadata(existing).ok()?.dev();
    let (major, minor) = (libc::major(dev), libc::minor(dev));
    crypt_below(&PathBuf::from(format!("/sys/dev/block/{major}:{minor}")), 0)
}

/// Walks a block device and the devices it sits on, looking for a dm-crypt one (wa_run.rs:449
/// `crypt_below`, the name left out).
fn crypt_below(sys: &Path, depth: u32) -> Option<String> {
    if depth > 6 {
        return None;
    }
    if let Ok(uuid) = std::fs::read_to_string(sys.join("dm/uuid"))
        && uuid.starts_with("CRYPT-")
    {
        return Some("dm-crypt".to_owned());
    }
    // A partition has no `slaves` of its own: its disk is the parent directory.
    let slaves = std::fs::read_dir(sys.join("slaves")).ok()?;
    slaves
        .flatten()
        .find_map(|slave| crypt_below(&slave.path(), depth + 1))
}

// ---- Where a profile is -------------------------------------------------------------------------

/// Where a profile is, as the command line says it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Where {
    pub profile: Option<String>,
    /// The rehearsal's throwaway place: a profile directory `<data home>/kourier-tg-run/<name>`
    /// below `<target>/tgr/` (the app's own run-profile rule holds there too, with the data home as
    /// its `XDG_DATA_HOME`). Refused anywhere else.
    pub rehearsal_place: Option<PathBuf>,
}

/// A resolved profile directory.
#[derive(Clone, Debug)]
pub(super) struct ProfileDir {
    /// The directory (the app's `KOURIER_HOME`).
    pub dir: PathBuf,
    /// The rehearsal's throwaway place, not a kept profile.
    pub rehearsal: bool,
}

impl Where {
    /// The directory this names (wa_run.rs:672 `resolve`). `target` is the cargo target directory
    /// (the rehearsal's places are below it).
    pub fn resolve(
        &self,
        vars: &dyn Fn(&str) -> Option<String>,
        target: &Path,
    ) -> Result<ProfileDir> {
        match (&self.profile, &self.rehearsal_place) {
            (Some(_), Some(_)) => bail!("give one of --profile and --rehearsal-place"),
            (None, Some(place)) => {
                let dir = std::path::absolute(place)
                    .with_context(|| format!("--rehearsal-place {}", place.display()))?;
                let base = target.join(REHEARSAL_DIR);
                let in_a_keeper_root = dir
                    .parent()
                    .and_then(Path::file_name)
                    .is_some_and(|n| n == KEEPER_DIR);
                let named = dir
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(valid_profile_name);
                if dir
                    .components()
                    .any(|c| c == std::path::Component::ParentDir)
                    || !dir.starts_with(&base)
                    || dir == base
                    || !in_a_keeper_root
                    || !named
                {
                    bail!(
                        "--rehearsal-place must be a profile directory <data home>/{KEEPER_DIR}/<name> below {}: the rehearsal never uses another place",
                        base.display()
                    );
                }
                Ok(ProfileDir {
                    dir,
                    rehearsal: true,
                })
            }
            (name, None) => {
                let name = name.as_deref().unwrap_or(super::DEFAULT_PROFILE);
                if !valid_profile_name(name) {
                    bail!(
                        "invalid profile name (1-32 letters, digits, '-' or '_', no run of 7 digits, never the API file's name)"
                    );
                }
                let dir = keeper_root(vars)?.join(name);
                // A profile is a directory. Anything else of that name in the keeper root (a file, a
                // link) is not one, and is not touched: the API file's name is the case that matters.
                if std::fs::symlink_metadata(&dir).is_ok_and(|m| !m.is_dir()) {
                    bail!(
                        "a file of that name stands in the keeper root, and a profile is a directory: use another profile name. Nothing was started."
                    );
                }
                Ok(ProfileDir {
                    dir,
                    rehearsal: false,
                })
            }
        }
    }
}

impl ProfileDir {
    /// The profile's NAME, never its path (a path holds the home directory and so the user's name;
    /// the report is meant to be pasted into a chat; wa_run.rs:712 `label`). A rehearsal's place
    /// says so.
    pub fn label(&self) -> String {
        let name = self
            .dir
            .file_name()
            .map_or_else(|| "?".to_owned(), |n| n.to_string_lossy().into_owned());
        if self.rehearsal {
            format!("{name} (rehearsal)")
        } else {
            name
        }
    }

    /// The keeper root this profile is a child of.
    pub fn keeper(&self) -> &Path {
        self.dir.parent().unwrap_or(&self.dir)
    }

    /// The tool's own files (`state.json`, the lock, later the stamped copies, the logs).
    pub fn tool(&self) -> PathBuf {
        self.dir.join(TOOL_DIR)
    }

    pub fn state_file(&self) -> PathBuf {
        self.tool().join("state.json")
    }

    /// The app's own tree inside the profile (`KOURIER_HOME`'s `kourier` directory).
    pub fn app_home(&self) -> PathBuf {
        self.dir.join("kourier")
    }

    /// The app's preferences (`kourier-app/src/paths.rs`: `<home>/kourier/config/kourier.toml`).
    pub fn config_file(&self) -> PathBuf {
        self.app_home().join("config").join("kourier.toml")
    }

    /// Held by a `connect` for as long as it runs: one run per profile; `wipe` asks it.
    pub fn lock_file(&self) -> PathBuf {
        self.tool().join("lock")
    }
}

// ---- The places a login must not live in ----------------------------------------------------------

/// What the places a login must not live in are: the repository and its worktrees, the build
/// directory.
pub(super) struct Places<'a> {
    pub root: &'a Path,
    pub target: &'a Path,
    pub worktrees: &'a [PathBuf],
}

/// Why `dir` must not hold a login, if anything (wa_run.rs:774 `unsafe_place`, with this tool's
/// rows): inside the repository, one of its worktrees or a build directory (where `git clean`,
/// `cargo clean` or a tidy-up reaches it), inside the app's own data directory or another run tool's
/// kept profiles (Signal's smoke tool, the WhatsApp and the Threema run tools), a base directory of
/// the user's own files, or a place a normal profile lives in.
pub(super) fn unsafe_place(
    dir: &Path,
    vars: &dyn Fn(&str) -> Option<String>,
    places: &Places<'_>,
) -> Option<String> {
    let mut inside: Vec<(&str, PathBuf)> = vec![
        ("the repository", places.root.to_path_buf()),
        ("the cargo target directory", places.target.to_path_buf()),
    ];
    inside.extend(
        places
            .worktrees
            .iter()
            .map(|w| ("a worktree of the repository", w.clone())),
    );
    let home = vars("HOME").map(PathBuf::from);
    let xdg = |var: &str, fallback: &str| {
        vars(var)
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| home.as_ref().map(|h| h.join(fallback)))
    };
    let data = xdg("XDG_DATA_HOME", ".local/share");
    if let Some(data) = &data {
        inside.push(("the app's own data directory", data.join("kourier")));
        inside.push((
            "the Signal smoke tool's kept profiles",
            data.join("kourier-smoke"),
        ));
        inside.push((
            "the WhatsApp run tool's kept profiles",
            data.join("kourier-wa-run"),
        ));
        inside.push((
            "the Threema run tool's kept profiles",
            data.join("kourier-threema-run"),
        ));
    }
    for (what, base) in &inside {
        if dir.starts_with(base) {
            return Some(format!(
                "that place is inside {what}: a clean-up or the other tool would reach the login"
            ));
        }
    }
    let bases = [
        home.clone(),
        data,
        xdg("XDG_CONFIG_HOME", ".config"),
        xdg("XDG_STATE_HOME", ".local/state"),
        xdg("XDG_CACHE_HOME", ".cache"),
    ];
    if bases.iter().flatten().any(|b| b == dir) {
        return Some(
            "that place is a base directory of the user's own files: Kourier's normal profile would share it"
                .to_owned(),
        );
    }
    // A normal profile keeps `core.db` and `accounts/` directly in `<data home>/kourier`.
    let normal = dir.join("kourier");
    if normal.join("core.db").exists() || normal.join("accounts").exists() {
        return Some("that place already holds a normal Kourier profile".to_owned());
    }
    None
}

// ---- The tool's state file ----------------------------------------------------------------------

/// `<profile>/tg-run/state.json`: what the tool knows about the profile (D-13):
/// `{"version":1,"env":"test","connects":[{"at_ms","verdict"}]}`. Never a number, a name, a tail,
/// a path or a text.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct State(Value);

impl State {
    /// A fresh profile's state, bound to the servers `env` names.
    pub fn new(env: &str) -> Self {
        Self(json!({ "version": 1, "env": env, "connects": [] }))
    }

    /// The state file of `dir`: `None` when there is none, an error when it is not this tool's
    /// (so a hand-written or damaged file is refused, never adopted).
    pub fn load(dir: &ProfileDir) -> Result<Option<Self>> {
        let Some(value) = read_json(&dir.state_file())? else {
            return Ok(None);
        };
        let state = Self(value);
        if state.0["version"] != json!(1)
            || state.0["env"].as_str().is_none()
            || !state.0["connects"].is_array()
        {
            bail!(
                "the state file of this profile is not tg-run's (version 1 with an env and a connects list): it is not adopted"
            );
        }
        Ok(Some(state))
    }

    pub fn save(&self, dir: &ProfileDir) -> Result<()> {
        write_private(&dir.state_file(), &format!("{:#}\n", self.0))
    }

    /// The servers the profile is bound to.
    pub fn env(&self) -> Option<&str> {
        self.0["env"].as_str()
    }

    /// Records one `connect`: when (ms since the epoch) and its verdict word (`PASS` or `FAIL`).
    /// Nothing else of a run is kept here.
    pub fn record_connect(&mut self, at_ms: u64, verdict: &str) {
        if let Some(list) = self.0["connects"].as_array_mut() {
            list.push(json!({ "at_ms": at_ms, "verdict": verdict }));
        }
    }

    /// The verdict words of the recorded connects, oldest first.
    #[cfg(test)]
    pub fn connect_verdicts(&self) -> Vec<String> {
        self.0["connects"]
            .as_array()
            .map(|l| {
                l.iter()
                    .filter_map(|c| c["verdict"].as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// Makes the profile's directories (0700, the keeper root too) and, for a fresh profile, its state
/// file (0600). Called once the preflight has passed, never before.
pub(super) fn ensure(dir: &ProfileDir, state: &State) -> Result<()> {
    private_dir(dir.keeper())?;
    private_dir(&dir.dir)?;
    private_dir(&dir.tool())?;
    state.save(dir)
}

// ---- The kourier.toml switches -------------------------------------------------------------------

/// What a profile's `kourier.toml` says about the networks and the one-shot link page (wa_run.rs:1229
/// `read_switches`, with the value of `pending-link` kept): the names of `networks-on` as written
/// (`None` when there is no such line) and the value of `pending-link` (`None` when there is no such
/// line). A line this cannot read counts as a switch that names nothing: the safe reading of a file
/// the tool does not understand is "something else is there".
pub(super) fn read_switches(text: &str) -> (Option<Vec<String>>, Option<String>) {
    let mut names = None;
    let mut pending = None;
    for line in text.lines() {
        let line = line.trim();
        let key_of = |key: &str| {
            line.strip_prefix(key)
                .map(str::trim_start)
                .and_then(|r| r.strip_prefix('='))
        };
        if let Some(value) = key_of("pending-link") {
            pending = Some(value.trim().trim_matches(['"', '\'']).to_owned());
        } else if let Some(value) = key_of("networks-on") {
            names = Some(
                value
                    .trim()
                    .strip_prefix('[')
                    .and_then(|list| list.split(']').next())
                    .map(|list| {
                        list.split(',')
                            .map(|n| n.trim().trim_matches(['"', '\'']).to_owned())
                            .filter(|n| !n.is_empty())
                            .collect()
                    })
                    .unwrap_or_default(),
            );
        }
    }
    (names, pending)
}

// ---- wipe -----------------------------------------------------------------------------------------

/// A lock taken with `File::try_lock` and asked again until `wait` has passed (wa_run's
/// `try_lock_for`, smoke.rs:1569): a lock that a program another thread just started carries until
/// its `exec` is not a lock held. The wait is a parameter, so a test passes none.
fn try_lock_for(file: &std::fs::File, wait: Duration) -> Result<bool> {
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

/// `wipe`: deletes the profile directory, only with the flag, and only a directory that is a
/// profile of this tool (it has the tool's state file) and is not in use (wa_run.rs:4318 `wipe_in`).
/// It never touches the keeper root's other entries: HIS API file stays. Says what it did and did
/// not do in words (the profile's NAME, never a path). The status is 0 when the profile was
/// deleted, 1 when nothing was.
pub(super) fn wipe_in(dir: &ProfileDir, yes: bool, settle: Duration) -> Result<(Vec<String>, i32)> {
    // Before anything is looked at or written: the API file's name, or any name no profile has.
    if !dir
        .dir
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(valid_profile_name)
    {
        bail!("that is not a profile name of this tool's: nothing is deleted");
    }
    let meta = std::fs::symlink_metadata(&dir.dir);
    if !meta.as_ref().is_ok_and(|m| m.is_dir()) {
        bail!(
            "profile {} is not a directory of this tool's (missing, or a link): nothing is deleted",
            dir.label()
        );
    }
    let state = State::load(dir)?.with_context(|| {
        format!(
            "profile {} is not a tg-run profile (no state file): nothing is deleted",
            dir.label()
        )
    })?;
    let mut lines = vec![format!(
        "wipe:     profile {} (env {}). This deletes the profile directory for good: the app's data, the stamped copies and the logs. The API file of the keeper root is not touched.",
        dir.label(),
        state.env().unwrap_or("?")
    )];
    if !yes {
        lines.push(
            "Nothing was deleted. Run this again with --yes-delete-the-login to delete the profile."
                .to_owned(),
        );
        return Ok((lines, 1));
    }
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(dir.lock_file())?;
    if !try_lock_for(&lock, settle)? {
        bail!("a connect is running on this profile: nothing is deleted");
    }
    std::fs::remove_dir_all(&dir.dir)
        .with_context(|| format!("delete the profile {}", dir.label()))?;
    lines.push("deleted.".to_owned());
    Ok((lines, 0))
}
