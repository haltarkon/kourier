//! The kept profile of `threema-run`: where it lives, the places it must not live in, the names it
//! may have, the private files, the tool's state file and `wipe`.
//!
//! The keeper root is `${XDG_DATA_HOME:-~/.local/share}/kourier-threema-run` (0700, outside the
//! repository, never the app's own data directory); it holds one directory per profile, which is the
//! app's `KOURIER_HOME`, the list of test IDs (`threema-allowed-ids`, seconds long and only in a
//! `--for-link` start), the keeper lock, the open question of a failed link and the backups.
//! Everything of the tool's own is below `<profile>/threema-run/`.
//!
//! Every helper here is a COPY of `wa_run.rs` or `tg_run/profile.rs` (each names its source as
//! `(wa_run.rs:<line> <fn>)`; this is the fourth copy, C5): the run tools are not shared before the
//! extraction `.claude/rules/dev-runs.md` lists, so a change to one cannot move another.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Write as _;
use std::os::unix::fs::{
    DirBuilderExt as _, MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _,
};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

use super::{
    ALLOW_FILE, ALLOW_TMP, BACKUPS_DIR, DEFAULT_PROFILE, KEEPER_DIR, KEEPER_LOCK,
    OPEN_QUESTION_FILE, REHEARSAL_DIR, TOOL_DIR, looks_like_an_id,
};

/// Creates `dir` with mode 0700 (and missing parents) and tightens an existing one (wa_run.rs:445
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

/// Makes the directory entry changes of `dir` durable: a created, renamed or removed file stays so
/// after a power loss. A bare name is the current directory.
pub(super) fn sync_dir(dir: &Path) -> Result<()> {
    let dir = if dir.as_os_str().is_empty() {
        Path::new(".")
    } else {
        dir
    };
    std::fs::File::open(dir)
        .and_then(|d| d.sync_all())
        .with_context(|| format!("sync {}", dir.display()))
}

/// Writes `text` to `path` (mode 0600) through a temporary file, an fsync, a rename and an fsync of
/// the directory (wa_run.rs:460 `write_private`).
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
    sync_dir(path.parent().unwrap_or(Path::new(".")))
}

/// Appends one line to `path` (mode 0600 when it is made; wa_run.rs:487 `append_line`).
pub(super) fn append_line(path: &Path, line: &str) -> Result<()> {
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("append to {}", path.display()))?;
    writeln!(f, "{line}")?;
    Ok(())
}

/// Removes the file `path` and makes the removal durable (the directory is fsynced). `true` when
/// there was a file, `false` when there was none. A directory of that name is an error.
pub(super) fn remove_synced(path: &Path) -> Result<bool> {
    match std::fs::remove_file(path) {
        Ok(()) => {
            sync_dir(path.parent().unwrap_or(Path::new(".")))?;
            Ok(true)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).with_context(|| format!("remove {}", path.display())),
    }
}

/// A JSON file; `None` when it is not there (wa_run.rs:504 `read_json`).
pub(super) fn read_json(path: &Path) -> Result<Option<Value>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(
            serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))?,
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

/// A profile name (wa_run.rs:515 `valid_profile_name`). Additions, because the profile's NAME is the
/// one thing of the profile the tool prints: a name holding seven or more digits in a row is
/// refused (a phone number given as a name would reach a report), so is a name of exactly eight
/// letters and digits (the shape of a Threema ID, in either case: the ID would reach a report and a
/// directory listing), the allow list's own file name (`threema-allowed-ids` has the characters of
/// a name, and a profile of that name would BE the list) and `backups` (the default place of
/// `backup`, a direct child of the keeper root that must never be a profile).
pub(super) fn valid_profile_name(name: &str) -> bool {
    (1..=32).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        && !holds_a_number(name)
        && !looks_like_an_id(name)
        && name != ALLOW_FILE
        && name != BACKUPS_DIR
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

/// The directory of the kept profiles: `${XDG_DATA_HOME:-~/.local/share}/kourier-threema-run`
/// (wa_run.rs:525 `keeper_root`, with this tool's directory; the app's `keeper_root` is its twin and
/// a test pins the names). It takes the environment, so a test puts the whole thing below a scratch
/// directory.
pub(super) fn keeper_root(env: &dyn Fn(&str) -> Option<String>) -> Result<PathBuf> {
    let abs = |k: &str| env(k).map(PathBuf::from).filter(|p| p.is_absolute());
    let data = abs("XDG_DATA_HOME")
        .or_else(|| abs("HOME").map(|h| h.join(".local/share")))
        .ok_or_else(|| anyhow!("neither XDG_DATA_HOME nor HOME is set"))?;
    Ok(data.join(KEEPER_DIR))
}

/// What protects the directory at rest: `Some(..)` when the file system below `dir` is on a
/// dm-crypt device (LUKS), looked up from its device number in sysfs (wa_run.rs:536
/// `disk_encryption`). The answer says "dm-crypt" and no more: the device's name is not printed.
pub(super) fn disk_encryption(dir: &Path) -> Option<String> {
    let existing = dir.ancestors().find(|p| p.exists())?;
    let dev = std::fs::metadata(existing).ok()?.dev();
    let (major, minor) = (libc::major(dev), libc::minor(dev));
    crypt_below(&PathBuf::from(format!("/sys/dev/block/{major}:{minor}")), 0)
}

/// Walks a block device and the devices it sits on, looking for a dm-crypt one (wa_run.rs:545
/// `crypt_below`, the name left out).
pub(super) fn crypt_below(sys: &Path, depth: u32) -> Option<String> {
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
    /// The rehearsal's throwaway place: a profile directory `<data home>/kourier-threema-run/<name>`
    /// below `<target>/thr/` (the app's own run-profile rule holds there too, with the data home as
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
    /// The flags that name this place on another command line: the supervisor's argv and the line the
    /// user pastes for `id`. Always explicit (a default profile is named too), so the process they
    /// reach resolves the very directory `start` did (wa_run.rs:762 `flags`).
    pub fn flags(&self) -> Vec<String> {
        match (&self.profile, &self.rehearsal_place) {
            (_, Some(place)) => vec![
                "--rehearsal-place".to_owned(),
                std::path::absolute(place)
                    .unwrap_or_else(|_| place.clone())
                    .display()
                    .to_string(),
            ],
            (name, None) => vec![
                "--profile".to_owned(),
                name.clone().unwrap_or_else(|| DEFAULT_PROFILE.to_owned()),
            ],
        }
    }

    /// The directory this names (wa_run.rs:782 `resolve`, tg_run/profile.rs `resolve`). `target` is
    /// the cargo target directory (the rehearsal's places are below it).
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
                let name = name.as_deref().unwrap_or(DEFAULT_PROFILE);
                if !valid_profile_name(name) {
                    bail!(
                        "invalid profile name (1-32 letters, digits, '-' or '_'; no run of 7 digits, not the shape of an ID, not `{BACKUPS_DIR}`, never the list's file name)"
                    );
                }
                let dir = keeper_root(vars)?.join(name);
                // A profile is a directory. Anything else of that name in the keeper root (a file, a
                // link) is not one, and is not touched: the list's file name is the case that matters.
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
    /// the report is meant to be pasted into a chat; wa_run.rs:821 `label`). A rehearsal's place says
    /// so.
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

    /// The tool's own files (`state.json`, the logs, the stamped copy, the supervisor's files).
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

    /// Held by the supervisor for as long as it lives: one supervisor per profile; `wipe` asks it.
    pub fn lock_file(&self) -> PathBuf {
        self.tool().join("lock")
    }

    /// The app's stderr, redacted, line by line.
    pub fn app_log(&self) -> PathBuf {
        self.tool().join("app.log")
    }

    /// Every event the app's dev channel reported (sanitised), one JSON object per line.
    pub fn events_log(&self) -> PathBuf {
        self.tool().join("events.jsonl")
    }

    /// One record per step call.
    pub fn steps_log(&self) -> PathBuf {
        self.tool().join("steps.jsonl")
    }

    /// Who supervises: the supervisor's pid, the app's pid, when it started, where its socket is.
    pub fn supervisor_file(&self) -> PathBuf {
        self.tool().join("supervisor.json")
    }

    /// What the supervisor process itself printed (an error before the app existed).
    pub fn supervisor_log(&self) -> PathBuf {
        self.tool().join("supervisor.log")
    }

    /// The report `report` writes.
    pub fn report_file(&self) -> PathBuf {
        self.tool().join("report.md")
    }

    /// The list of test IDs the app reads (`threema-allowed-ids`, the app's `ALLOW_FILE`): in the
    /// KEEPER ROOT, shared by every profile of it.
    pub fn allow_file(&self) -> PathBuf {
        self.keeper().join(ALLOW_FILE)
    }

    /// The temporary file the list is written through (`write_private`'s `.tmp`): it holds the digest
    /// until the rename, so it is looked for and removed wherever the list is (LOW-1).
    pub fn allow_tmp(&self) -> PathBuf {
        self.keeper().join(ALLOW_TMP)
    }

    /// The keeper lock: held from the moment the list is written until it is gone, so two link
    /// starts of two profiles never overlap.
    pub fn keeper_lock_file(&self) -> PathBuf {
        self.keeper().join(KEEPER_LOCK)
    }

    /// The open question of a failed link, kept in the keeper root so that a `wipe` of the profile
    /// cannot lose it (critic M4).
    pub fn open_question_file(&self) -> PathBuf {
        self.keeper().join(OPEN_QUESTION_FILE)
    }

    /// The default place of `backup`: grandchildren of the keeper root, never a run profile.
    pub fn backups_dir(&self) -> PathBuf {
        self.keeper().join(BACKUPS_DIR)
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

/// Why `dir` must not hold a login, if anything (wa_run.rs:886 `unsafe_place`, tg_run/profile.rs, with
/// this tool's rows): inside the repository, one of its worktrees or a build directory (where `git
/// clean`, `cargo clean` or a tidy-up reaches it), inside the projects tree (it syncs to the server
/// every hour), inside the app's own data directory or another run tool's kept profiles (Signal's
/// smoke tool, the WhatsApp and the Telegram run tools), a base directory of the user's own files, or
/// a place a normal profile lives in. The words name the place, never its path.
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
    if let Some(home) = &home {
        inside.push((
            "the projects tree (it syncs to the server every hour)",
            home.join("projects"),
        ));
    }
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
            "the Telegram run tool's kept profiles",
            data.join("kourier-tg-run"),
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

/// Refuses a profile place that is not canonical (a link on its way): the app takes its run profile
/// only when `KOURIER_HOME` is canonical and a DIRECT child of the canonical keeper root
/// (`kourier-app/src/threema.rs` `run_profile`), and a profile it would not take would start as a
/// normal one. The deepest directory that exists is canonicalized and the rest is appended, so it
/// works before anything is made.
pub(super) fn check_canonical(dir: &Path) -> Result<()> {
    let existing = dir
        .ancestors()
        .find(|p| p.exists())
        .ok_or_else(|| anyhow!("no part of the profile's place exists"))?;
    let rest = dir.strip_prefix(existing).unwrap_or(Path::new(""));
    let canonical = std::fs::canonicalize(existing)
        .context("the profile's place")?
        .join(rest);
    if canonical != dir {
        bail!(
            "the profile's place is not canonical (a link or a `..` on its way): the app would not take it as a run profile. Use a data home with no link. Nothing was started."
        );
    }
    Ok(())
}

// ---- The tool's state file ----------------------------------------------------------------------

/// `<profile>/threema-run/state.json`: what the tool knows about the profile without opening its
/// encrypted databases (wa_run.rs:954 `State`). Link records (time, schemas, migration blob ids),
/// `halted` and `last_halt`, the start count, `link_at` (set while a login stands at `linking`, so
/// a supervisor that dies there is found at the next start, critic M4). NEVER the test ID, its
/// digest, a name, a number or a text.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct State(Value);

impl State {
    pub fn new(now: u64) -> Self {
        Self(json!({
            "version": 1,
            "created_ms": now,
            "starts": 0,
            "build_commit": null,
            // The migration files that ran on this profile: name -> git blob id (frozen).
            "migrations": {},
            "core_schema": null,
            "linked_since_ms": null,
            // The core.db schema the build had when the link finished.
            "core_schema_at_link": null,
            "last_online_ms": null,
            // Each attempt of a link: when, and one outcome word, never a value.
            "link_attempts": [],
            // The time the login reached `linking`, until it ended (`done` or a refusal).
            "link_at": null,
            "halted": null,
            "last_halt": null,
        }))
    }

    /// The state file of `dir`: `None` when there is none, an error when it is not this tool's
    /// (a hand-written or damaged file is refused, never adopted).
    pub fn load(dir: &ProfileDir) -> Result<Option<Self>> {
        let Some(value) = read_json(&dir.state_file())? else {
            return Ok(None);
        };
        let state = Self(value);
        if state.0["version"] != json!(1)
            || state.num("created_ms").is_none()
            || state.num("starts").is_none()
            || !state.0["migrations"].is_object()
            || !state.0["link_attempts"].is_array()
        {
            bail!(
                "the state file of this profile is not threema-run's (version 1 with created_ms, starts, migrations and link_attempts): it is not adopted"
            );
        }
        Ok(Some(state))
    }

    pub fn save(&self, dir: &ProfileDir) -> Result<()> {
        write_private(&dir.state_file(), &format!("{:#}\n", self.0))
    }

    pub fn num(&self, key: &str) -> Option<u64> {
        self.0[key].as_u64()
    }

    /// The value under `key` (`Value::Null` when there is none).
    pub fn get(&self, key: &str) -> &Value {
        &self.0[key]
    }

    pub fn set(&mut self, key: &str, value: Value) {
        self.0[key] = value;
    }

    /// A link finished here (or an account that was linked before is online again).
    pub fn linked(&self) -> bool {
        self.num("linked_since_ms").is_some()
    }

    /// The TEST phone's adb serial the run keeps (`start --test-phone`), if any.
    pub fn test_phone(&self) -> Option<&str> {
        self.0["test_phone"].as_str().filter(|s| !s.is_empty())
    }

    /// A login reached `linking` and its end was not seen: a device MAY be registered.
    pub fn link_stood(&self) -> bool {
        !self.0["link_at"].is_null()
    }

    /// The kind word of the run's halt, if it was halted and not yet started again.
    pub fn halt_kind(&self) -> Option<&str> {
        self.0["halted"]["kind"].as_str()
    }

    /// The migration files that ran here, with the blob id each had.
    pub fn migrations(&self) -> BTreeMap<String, String> {
        self.0["migrations"]
            .as_object()
            .map(|m| {
                m.iter()
                    .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_owned())))
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// Makes the profile's directories (0700, the keeper root too) and, for a fresh profile, its state
/// file (0600). Called once the preflight has passed, never before; a place that is not canonical is
/// refused before anything is made.
pub(super) fn ensure(dir: &ProfileDir, state: &State) -> Result<()> {
    check_canonical(&dir.dir)?;
    private_dir(dir.keeper())?;
    private_dir(&dir.dir)?;
    private_dir(&dir.tool())?;
    state.save(dir)
}

// ---- wipe -----------------------------------------------------------------------------------------

/// A lock taken with `File::try_lock` and asked again until `wait` has passed (wa_run's
/// `try_lock_for`, smoke.rs:1569): a lock that a program another thread just started carries until
/// its `exec` is not a lock held. The wait is a parameter, so a test passes none.
pub(super) fn try_lock_for(file: &std::fs::File, wait: Duration) -> Result<bool> {
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
            Err(std::fs::TryLockError::Error(e)) => return Err(e).context("lock"),
        }
    }
}

/// A lock file made (0600) and opened for locking.
pub(super) fn open_lock_file(path: &Path) -> Result<std::fs::File> {
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("open {}", path.display()))
}

/// `wipe`: deletes the profile directory (the login with it), only with the flag, and only a
/// directory that is a profile of this tool (it has the tool's state file) and is not in use
/// (wa_run.rs:6810 `wipe_in`, tg_run/profile.rs `wipe_in`). It says first what the phone still
/// holds. With the flag it refuses while the open question of a failed link stands in the keeper
/// root, and while the state says a login stood at `linking` (a device MAY be registered and
/// nobody has looked: the question must be answered first, critic M4). It never touches the keeper
/// root's other entries and the backups; a leftover list of test IDs it removes, unless another run
/// holds the keeper lock (that list is not left over). The words never name a path; the status is
/// 0 when the profile was deleted, 1 when nothing was.
pub(super) fn wipe_in(dir: &ProfileDir, yes: bool, settle: Duration) -> Result<(Vec<String>, i32)> {
    // Before anything is looked at or written: the list's file name, `backups`, or any name no
    // profile has.
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
            "profile {} is not a threema-run profile (no state file): nothing is deleted",
            dir.label()
        )
    })?;
    let mut lines = vec![format!(
        "wipe:     profile {}. This deletes the profile directory for good: the app's data (the account's keys with it), the stamped copy and the logs. The keeper root's other entries and the backups are not touched.",
        dir.label()
    )];
    lines.push(format!(
        "login:    {}",
        if state.linked() {
            "an account was linked on this profile"
        } else {
            "no account was linked on this profile"
        }
    ));
    lines.push(
        "phone:    the TEST phone keeps listing the linked device until it is removed there (Threema's linked devices on the TEST phone, the entry Kourier made). Do that first; a wiped profile can no longer drop its own device. The personal phone never opens that list."
            .to_owned(),
    );
    if !yes {
        lines.push(
            "Nothing was deleted. Run this again with --yes-delete-the-login after the device is removed on the TEST phone. Backups made with `backup` are separate directories and stay."
                .to_owned(),
        );
        return Ok((lines, 1));
    }
    if std::fs::symlink_metadata(dir.open_question_file()).is_ok() {
        bail!(
            "the question of a failed link is still open (a device may be registered on the TEST phone): answer it first, then wipe. Nothing is deleted"
        );
    }
    if state.link_stood() {
        bail!(
            "a login of this profile stood at `linking` and its end was not seen (a device may be registered on the TEST phone): start the profile once more so the tool asks, answer, then wipe. Nothing is deleted"
        );
    }
    let lock = open_lock_file(&dir.lock_file())?;
    if !try_lock_for(&lock, settle)? {
        bail!("the app is running on this profile: `threema-run stop` first. Nothing is deleted");
    }
    // The app's own lock too (LOW-5, as `backup`): the user may have started the kept profile by hand
    // to read its chats, or an app outlives a supervisor that was killed; its databases and keys must
    // not go under it.
    if matches!(
        super::preflight::probe_lock(&super::preflight::app_lock_file(dir), settle)?,
        super::preflight::Lock::Held
    ) {
        bail!(
            "an app still holds its own lock on profile {} (its supervisor is gone, or it was started by hand): `threema-run stop` first. Nothing is deleted",
            dir.label()
        );
    }
    std::fs::remove_dir_all(&dir.dir)
        .with_context(|| format!("delete the profile {}", dir.label()))?;
    lines.extend(remove_leftover_list(dir)?);
    lines.push("deleted.".to_owned());
    Ok((lines, 0))
}

/// Removes a list of test IDs a run left in the keeper root, and the temporary file it was written
/// through (LOW-1: it holds the digest too): `unlink` and an fsync of the directory, unless another
/// run holds the keeper lock: a list under a live lock belongs to that run. The words say what was
/// done.
pub(super) fn remove_leftover_list(dir: &ProfileDir) -> Result<Vec<String>> {
    let (list, tmp) = (dir.allow_file(), dir.allow_tmp());
    if std::fs::symlink_metadata(&list).is_err() && std::fs::symlink_metadata(&tmp).is_err() {
        return Ok(Vec::new());
    }
    let keeper = open_lock_file(&dir.keeper_lock_file())?;
    if !try_lock_for(&keeper, Duration::ZERO)? {
        return Ok(vec![
            "warning: the list of test IDs stands in the keeper root and another run holds the keeper lock: it is left to that run."
                .to_owned(),
        ]);
    }
    // The temporary file first: it is the one nothing else knows about.
    let had_tmp = remove_synced(&tmp).context("remove the leftover temporary list of test IDs")?;
    let had_list = remove_synced(&list).context("remove the leftover list of test IDs")?;
    Ok(vec![format!(
        "warning: {} left in the keeper root by a run that did not end cleanly; removed.",
        match (had_list, had_tmp) {
            (true, true) => "a list of test IDs and its temporary file were",
            (false, true) => "a temporary file of the list of test IDs was",
            _ => "a list of test IDs was",
        }
    )])
}
