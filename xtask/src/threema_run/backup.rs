//! `threema-run backup` (T10): a copy of the kept profile while NO app runs on it.
//!
//! The copy goes to `<keeper root>/backups/<profile>-<UTC time>/` (0700, the reserved name `backups`:
//! a grandchild of the keeper root, never a profile, so a hand start of it reads no list), or with
//! `--to <dir>` to `<dir>/<profile>-<UTC time>/` (`<dir>` an existing directory). It is refused:
//!
//! * while the profile's lock is held (a supervisor is up) or the app's own lock is (a supervisor that
//!   was killed leaves the app running: `stop` first), because a copy of a database that is written
//!   to is no copy;
//! * for a place the repository, its worktrees, `~/projects` (it syncs to the server every hour), the
//!   build directory, the app's own data directory, another run tool's profiles or a base directory
//!   of the user's own files would hold ([`super::profile::unsafe_place`]), a place inside the profile
//!   itself, a direct child of the keeper root (it would look like a profile), a path with `..` or a
//!   link on its way, and a disk without encryption unless `--allow-unencrypted-disk` (the copy holds
//!   the account's keys under a public database key);
//! * for a directory that is no profile of this tool's (no state file) and a name no profile has.
//!
//! What is copied: every regular file of the profile, directories 0700, files with their modes, each
//! fsynced. NOT copied: the list of test IDs (`threema-allowed-ids` is the keeper root's, never part of
//! a profile, and a file of that name anywhere in the tree is left out too), the stamped copy of the
//! app (`threema-run/bin`: execute-only, 0.9 GB, holds no login), the app's plaintext playback copies
//! (`kourier/run`), the supervisor's files of a run that is over, and anything that is not a regular
//! file (a socket, a link, a pipe). A copy that fails half way is removed. The words name the profile
//! and the copy by NAME, never a path (a path holds the user's name).

use std::fs::OpenOptions;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use super::preflight::{Lock, app_lock_file, probe_lock, utc_compact};
use super::profile::{
    Places, ProfileDir, State, check_canonical, open_lock_file, private_dir, sync_dir,
    try_lock_for, unsafe_place, valid_profile_name,
};
use super::{ALLOW_FILE, Args, TOOL_DIR, var};
use crate::target_dir;

/// How long `backup` waits for a lock another thread just let go of (the supervisor's, the app's).
const BACKUP_SETTLE: Duration = Duration::from_millis(400);

/// What the places around the copy are (injected: the tests use scratch places).
pub(super) struct BackupEnv<'a> {
    pub vars: &'a dyn Fn(&str) -> Option<String>,
    pub root: &'a Path,
    pub target: &'a Path,
    pub worktrees: &'a [PathBuf],
    /// Whether a place is on an encrypted disk (`profile::disk_encryption`).
    pub disk: &'a dyn Fn(&Path) -> Option<String>,
    pub now_ms: u64,
    pub settle: Duration,
}

/// What a copy holds, in counts.
#[derive(Default)]
struct Copied {
    files: usize,
    skipped: usize,
}

/// Whether a path below the profile (relative to it) is left out of the copy.
fn left_out(rel: &Path) -> bool {
    rel == Path::new(TOOL_DIR).join("bin")
        || rel == Path::new("kourier").join("run")
        || rel == Path::new(TOOL_DIR).join("lock")
        || rel == Path::new(TOOL_DIR).join("supervisor.json")
        || rel.file_name().is_some_and(|n| n == ALLOW_FILE)
}

/// Copies the tree below `from` into `to` (made 0700): regular files only, each fsynced, the
/// directories fsynced after their entries.
fn copy_tree(from: &Path, to: &Path, base: &Path, out: &mut Copied) -> Result<()> {
    private_dir(to)?;
    for entry in std::fs::read_dir(from).with_context(|| format!("read {}", from.display()))? {
        let entry = entry?;
        let path = entry.path();
        let rel = path.strip_prefix(base).unwrap_or(&path);
        let kind = entry.file_type()?;
        if left_out(rel) || !(kind.is_dir() || kind.is_file()) {
            out.skipped += 1;
            continue;
        }
        let target = to.join(entry.file_name());
        if kind.is_dir() {
            copy_tree(&path, &target, base, out)?;
        } else {
            std::fs::copy(&path, &target).with_context(|| "copy a file of the profile")?;
            OpenOptions::new()
                .write(true)
                .open(&target)
                .and_then(|f| f.sync_all())
                .with_context(|| "sync a copied file")?;
            out.files += 1;
        }
    }
    sync_dir(to)
}

/// `backup` over a resolved profile. Returns the words to print. Every refusal is an error that
/// says nothing was copied.
pub(super) fn backup_in(
    dir: &ProfileDir,
    to: Option<&Path>,
    allow_unencrypted: bool,
    env: &BackupEnv<'_>,
) -> Result<Vec<String>> {
    let name = dir
        .dir
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|n| valid_profile_name(n))
        .context("that is not a profile name of this tool's: nothing was copied")?
        .to_owned();
    if !std::fs::symlink_metadata(&dir.dir).is_ok_and(|m| m.is_dir()) {
        bail!(
            "profile {} is not a directory of this tool's (missing, or a link): nothing was copied",
            dir.label()
        );
    }
    State::load(dir)?.with_context(|| {
        format!(
            "profile {} is not a threema-run profile (no state file): nothing was copied",
            dir.label()
        )
    })?;
    // The app must not write while its databases are copied: the supervisor's lock (held until the
    // app's process is gone) and the app's own (a killed supervisor leaves the app running).
    let lock = open_lock_file(&dir.lock_file())?;
    if !try_lock_for(&lock, env.settle)? {
        bail!(
            "the app is running on profile {}: `threema-run stop` first. Nothing was copied",
            dir.label()
        );
    }
    if matches!(probe_lock(&app_lock_file(dir), env.settle)?, Lock::Held) {
        bail!(
            "an app still holds its own lock on profile {} (its supervisor is gone): `threema-run stop` first. Nothing was copied",
            dir.label()
        );
    }
    // Where the copy goes: `<keeper root>/backups` or the directory `--to` names.
    let default = to.is_none();
    let parent = match to {
        Some(to) => {
            if to.components().any(|c| c == Component::ParentDir) {
                bail!("--to may not hold `..`: name the directory itself. Nothing was copied");
            }
            let parent = std::path::absolute(to).context("--to")?;
            if !std::fs::metadata(&parent).is_ok_and(|m| m.is_dir()) {
                bail!("--to must name an existing directory. Nothing was copied");
            }
            parent
        }
        None => dir.backups_dir(),
    };
    let dest = parent.join(format!("{name}-{}", utc_compact(env.now_ms)));
    if dest.starts_with(&dir.dir) {
        bail!("a copy of the profile cannot live inside the profile. Nothing was copied");
    }
    if parent == dir.keeper() {
        bail!(
            "a copy never lives directly in the keeper root (it would look like a profile): use its `backups` directory, the default. Nothing was copied"
        );
    }
    let mut warnings = Vec::new();
    if !dir.rehearsal {
        let places = Places {
            root: env.root,
            target: env.target,
            worktrees: env.worktrees,
        };
        if let Some(why) = unsafe_place(&dest, env.vars, &places) {
            bail!("refusing to put a copy of a real login there: {why}. Nothing was copied");
        }
        if (env.disk)(&parent).is_none() {
            if !allow_unencrypted {
                bail!(
                    "no disk encryption was found for the copy's place: the copy holds the account's keys under a public database key. Choose a place on the encrypted disk, or pass --allow-unencrypted-disk if the user accepts that. Nothing was copied"
                );
            }
            warnings.push(
                "warning: --allow-unencrypted-disk: the copy is on a disk without encryption (the user accepted that)"
                    .to_owned(),
            );
        }
    }
    if default {
        private_dir(&parent)?;
    }
    check_canonical(&parent)?;
    if std::fs::symlink_metadata(&dest).is_ok() {
        bail!("that copy exists already (one a second): nothing was copied");
    }
    let mut copied = Copied::default();
    if let Err(error) = copy_tree(&dir.dir, &dest, &dir.dir, &mut copied) {
        // A half copy must not look like a whole one.
        let _ = std::fs::remove_dir_all(&dest);
        return Err(error.context("the copy failed and was removed"));
    }
    sync_dir(&parent)?;
    debug_assert_eq!(
        std::fs::metadata(&dest)?.permissions().mode() & 0o777,
        0o700
    );
    let leaf = dest
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    let place = if default {
        format!("the keeper root's backups directory, `{leaf}`")
    } else {
        format!("`{leaf}` in the directory you named")
    };
    let mut lines = warnings;
    lines.push(format!(
        "backup:   {place} ({} files; {} entries left out: the stamped copy of the app, the app's plaintext playback copies, the supervisor's files and anything that is no regular file). The list of test IDs is never part of a copy.",
        copied.files, copied.skipped
    ));
    lines.push(
        "          It holds the test account's keys (the identity's client key and the device group key) and is as sensitive as the profile itself. Restore only a copy taken after the last run, as a whole directory with the profile's own name: a copy older than the device's removal on the phone would bring back a login the mediator no longer knows."
            .to_owned(),
    );
    drop(lock);
    Ok(lines)
}

/// `backup` as the command line runs it.
pub(super) fn backup_command(a: &Args) -> Result<i32> {
    let target = target_dir();
    let dir = a.place.resolve(&var, &target)?;
    let root = crate::root();
    let worktrees = if dir.rehearsal {
        Vec::new()
    } else {
        super::preflight::worktrees_of(&super::preflight::RealGit { root: &root })?
    };
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
    let env = BackupEnv {
        vars: &var,
        root: &root,
        target: &target,
        worktrees: &worktrees,
        disk: &super::profile::disk_encryption,
        now_ms,
        settle: BACKUP_SETTLE,
    };
    for line in backup_in(&dir, a.to.as_deref(), a.allow_unencrypted, &env)? {
        println!("{line}");
    }
    Ok(0)
}
