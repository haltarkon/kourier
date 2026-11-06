//! The start's checks (`briefs/tr.md` §2.6 with the critic's changes), in this order, each its own
//! function that takes what it reads as an argument (the environment, the repository root, a [`Git`]
//! runner, the cgroup text, the disk check, the kernel's clock state, the lock probes), so a test can
//! make each refusal on its own and assert ITS text. Nothing is made, started or written here but
//! the answer: the profile's directories, the state file, the switch, the removal of a leftover list
//! of test IDs and the socket are the launcher's, after the last check has passed.
//!
//! | # | Check |
//! |---|---|
//! | 1 | not a test (a real start only) |
//! | 2 | `--confirmed-by-user`: his go in this session, for EVERY start (a real start only) |
//! | 3 | inside `kourier.slice`; a rehearsal also where `container` says it is the build container, and only where no network interface but `lo` is seen |
//! | 4 | the place (not the repository, a worktree, `~/projects`, another tool's profiles ...), the disk encrypted or `--allow-unencrypted-disk` (a real start) |
//! | 5 | a clean tree at `main`'s HEAD (a real start) |
//! | 6 | the halt: a halted profile needs `--after-halt` |
//! | 7 | `--for-link` only on a profile with no account and while no failed link's question is open; no `--for-link` only on a profile that holds an account |
//! | 8 | the migrations of HEAD against the profile's (`--allow-schema-change` after a `backup`) (a real start) |
//! | 9 | the clock synchronized: `adjtimex` not `TIME_ERROR`, `STA_UNSYNC` clear, `maxerror` under 16 s (critic R-L1) (a real start) |
//! | 10 | `Cargo.lock` at HEAD holds main's TLS bump, for EVERY entry of a name (critic R-L6) (a real start) |
//! | 11 | every line the tool reads from the app's log stands in HEAD's source, in its words (a real start) |
//! | 12 | the keeper root: no other Kourier and no other supervisor (on any profile for a link start, on this one otherwise), no keeper lock held; a leftover list of test IDs is reported |
//! | 13 | the switch planned (`networks-on` for the run, off at every end); a `pending-link` line refused |
//! | 14 | the control socket's place: `XDG_RUNTIME_DIR` (a real start) |
//!
//! A rehearsal (`--rehearsal-place`, inside `unshare -rn`, no network, the fake that plays Threema)
//! keeps 3, 6, 7, 12 and 13 and skips the rest: it has no go (the run tool's walk is the order), no
//! disk to protect, a tree that is being built, no real server's clock or certificates, no line of
//! the real backend and a socket place of its own.

use std::collections::BTreeMap;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::Value;

use super::profile::{Places, ProfileDir, State, unsafe_place, valid_profile_name};
use super::{ALLOW_FILE, Args};
use crate::lock;

/// Variables that mean "this is a test run" (a real start refuses to start inside one). Exact names:
/// `NEXTEST_TEST_THREADS` is kcap's and says nothing (wa_run.rs:194 `TEST_ENV`).
pub(super) const TEST_ENV: &[&str] = &[
    lock::HELD_ENV,
    "NEXTEST",
    "NEXTEST_RUN_ID",
    "NEXTEST_EXECUTION_MODE",
];

/// The migration directories whose files a run freezes once they ran on a profile: (label,
/// directory). A Threema profile has the core's, the outbox's and the backend's (wa_run.rs:184
/// `MIGRATION_DIRS`, with Threema's backend).
pub(super) const MIGRATION_DIRS: [(&str, &str); 3] = [
    ("core", "crates/kourier-core/migrations"),
    ("outbox", "crates/kourier-db/migrations"),
    ("threema", "crates/kourier-threema/migrations"),
];

/// The name of the network in `networks-on`, as the app compares it (`NetworkKind::as_str`; the
/// compare ignores the case).
pub(super) const NETWORK_NAME: &str = "Threema";

/// Check 9: the kernel's estimate of the clock's error (`maxerror`, in microseconds) must stay below
/// the limit at which the kernel itself sets `STA_UNSYNC` (16 s: `timedatectl`'s own rule, critic
/// R-L1). The backend closes a connection over 20 minutes of skew, so nothing tighter is needed; a
/// bound of 1 s refused a synchronized PC about once in 45 starts.
pub(super) const MAXERROR_LIMIT_US: i64 = 16_000_000;

/// Check 10: the packages of `Cargo.lock` whose EVERY entry must be at least this version, with
/// the reason each one is here: the TLS bump of main (decisions item 1). `webpki-root-certs` is
/// bumped by it too and is NOT read: Threema's dial pins through `webpki-roots` (critic R-L6).
pub(super) const TLS_MINIMUMS: [(&str, &str); 3] = [
    ("rustls", "0.23.46"),
    ("rustls-webpki", "0.103.16"),
    ("webpki-roots", "1.0.11"),
];

/// Check 14: the control socket is `<XDG_RUNTIME_DIR>/kthr-<16 hex>.sock` (the hex is the launcher's
/// choice; the length is fixed): the directory's path plus this many bytes must stay within the
/// socket path's limit, so the place never depends on the checkout's length.
pub(super) const SOCKET_NAME_LEN: usize = 1 + "kthr-".len() + 16 + ".sock".len();
/// `sockaddr_un.sun_path` less its terminator (wa_run.rs:2182 `SOCKET_PATH_MAX`).
pub(super) const SOCKET_PATH_MAX: usize = 107;

/// How long a probe asks again for a lock that is held: a program another thread just started holds
/// a copy of the descriptor until its `exec` (`lock::SETTLE`, wa_run.rs:1611 `test_lock_busy`).
pub(super) const PROBE_SETTLE: Duration = Duration::from_millis(400);

// ---- Line check 11's table -----------------------------------------------------------------------

/// THE TABLE of check 11 is T5's `lines.rs` (one source for this check, the supervisor's reader and
/// the judge): every line TR reads, in the words of TH6's line contract (`conn/lines.rs`; the shapes
/// field by field are in `target/threema/th6-lines.md`) plus E6. A renamed or reworded line is a
/// halt rule that never fires, so a start refuses a HEAD that lacks one, and a unit test finds every
/// row in the working tree's source, so TH6's change of a word is caught the day it is made.
pub(super) use super::lines::{LINES, LineRow};

// ---- The seams ------------------------------------------------------------------------------------

/// A read-only runner of git commands in the repository (a seam: the tests fake it; wa_run.rs:905
/// `Git`, tg_run/preflight.rs `Git`).
pub(super) trait Git {
    fn run(&self, args: &[&str]) -> Result<String>;
}

/// `git -C <root> ...`, with no optional locks, so `status` does not write the index (wa_run.rs:910
/// `RealGit`).
pub(super) struct RealGit<'a> {
    pub root: &'a Path,
}

impl Git for RealGit<'_> {
    fn run(&self, args: &[&str]) -> Result<String> {
        let out = Command::new("git")
            .arg("-C")
            .arg(self.root)
            .args(args)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .output()
            .context("run git")?;
        if !out.status.success() {
            bail!(
                "git {} failed: {}",
                args.first().copied().unwrap_or(""),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

/// What `adjtimex` said (a read: `modes` 0, no privilege, nothing is set).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Clock {
    /// The call's return value: the clock state (`TIME_OK` 0 ... `TIME_ERROR` 5).
    pub state: i32,
    /// `timex.status`: the `STA_*` bits.
    pub status: i32,
    /// `timex.maxerror`: the kernel's estimate of the error, in microseconds.
    pub maxerror_us: i64,
}

/// The kernel's clock state (check 9's reader, as a function so a test injects the answer).
pub(super) fn read_clock() -> Result<Clock> {
    // SAFETY: `timex` is plain data; all-zero is a valid value, and `modes` 0 makes `adjtimex` only
    // read the state (it needs no privilege and sets nothing). The pointer is valid for the call.
    let (state, tx) = unsafe {
        let mut tx: libc::timex = std::mem::zeroed();
        let state = libc::adjtimex(&raw mut tx);
        (state, tx)
    };
    if state < 0 {
        bail!("adjtimex failed: {}", std::io::Error::last_os_error());
    }
    #[allow(
        clippy::useless_conversion,
        reason = "`c_long` is `i64` here and `i32` on 32-bit targets"
    )]
    let maxerror_us = i64::from(tx.maxerror);
    Ok(Clock {
        state,
        status: tx.status,
        maxerror_us,
    })
}

/// Whether a lock file is held by a process (the lock probes of check 12).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Lock {
    /// No file, or nobody holds it.
    Free,
    Held,
}

/// Looks at one lock file without taking it for good: a shared lock is refused while an exclusive
/// one stands and is let go at once; a file that is not there is free. Asked again for `settle`
/// (see [`PROBE_SETTLE`]). An error is an answer too: the caller refuses what it cannot see.
pub(super) fn probe_lock(path: &Path, settle: Duration) -> Result<Lock> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Lock::Free),
        Err(e) => return Err(e).context("open a lock file"),
    };
    let deadline = Instant::now() + settle;
    loop {
        match file.try_lock_shared() {
            Ok(()) => return Ok(Lock::Free),
            Err(std::fs::TryLockError::WouldBlock) => {
                if Instant::now() >= deadline {
                    return Ok(Lock::Held);
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(std::fs::TryLockError::Error(e)) => return Err(e).context("lock a lock file"),
        }
    }
}

/// Whether the test lock is held right now, without taking it: a warning, never a refusal
/// (wa_run.rs:1611 `test_lock_busy`).
pub(super) fn test_lock_busy() -> bool {
    lock::lock_candidates()
        .iter()
        .any(|path| probe_lock(path, PROBE_SETTLE).is_ok_and(|l| l == Lock::Held))
}

/// What the preflight reads.
pub(super) struct Ctx<'a> {
    /// The environment.
    pub vars: &'a dyn Fn(&str) -> Option<String>,
    /// The repository root and the cargo target directory.
    pub root: &'a Path,
    pub target: &'a Path,
    pub git: &'a dyn Git,
    /// The text of `/proc/self/cgroup`.
    pub cgroup: &'a str,
    /// `Some(device)` when the file system below the place is encrypted.
    pub disk: &'a dyn Fn(&Path) -> Option<String>,
    /// Whether the test lock is held (a probe: the lock is not taken).
    pub test_lock_busy: &'a dyn Fn() -> bool,
    /// The text of `/proc/self/net/dev`: the network interfaces this process sees.
    pub net_dev: &'a str,
    /// The uid the control socket's directory must belong to.
    pub uid: u32,
    /// Milliseconds since the epoch (a fresh profile's state is born with it).
    pub now_ms: &'a dyn Fn() -> u64,
    /// The kernel's clock state (`adjtimex`).
    pub clock: &'a dyn Fn() -> Result<Clock>,
    /// Whether a lock file is held.
    pub lock: &'a dyn Fn(&Path) -> Result<Lock>,
    /// The lines check 11 looks for (`LINES` in the tool, a table of their own in a test).
    pub lines: &'a [LineRow],
}

/// The switch of the run (check 13): the plan only, the launcher writes it before the app starts
/// and the supervisor's end writes it off (call C3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct SwitchPlan {
    /// The text of the profile's `kourier.toml` now (`None`: there is no file).
    pub existing: Option<String>,
    /// What the start writes: the same text with `networks-on` naming the network and nothing else
    /// changed (a missing file gets just that line).
    pub on_text: String,
    /// The file said the switch was ON already, while no supervisor and no app is alive on the
    /// profile (check 12 proved it): the last run did not end cleanly.
    pub found_on: bool,
}

/// What the preflight found.
#[derive(Debug)]
pub(super) struct Cleared {
    pub dir: ProfileDir,
    /// The profile's state: the file's, or a fresh one. With `--after-halt` the halt has moved to
    /// `last_halt` IN MEMORY: the launcher saves it once it starts.
    pub state: State,
    /// Lines to print before anything starts.
    pub warnings: Vec<String>,
    /// Whether the disk below the profile is encrypted; `None` in a rehearsal.
    pub encrypted: Option<bool>,
    /// main's HEAD the build is made from (a real start only).
    pub head: Option<String>,
    /// HEAD's migration files with their blob ids, to record (a real start only).
    pub migrations: Option<BTreeMap<String, String>>,
    /// `<keeper root>/threema-allowed-ids` stands and no run holds the keeper lock: the launcher
    /// removes it (unlink and directory fsync) and warns (the file's life, path 3).
    pub leftover_list: bool,
    /// A login of this profile reached `linking` and its end was not seen (`link_at`): a device MAY
    /// be registered on the TEST phone. The launcher opens the question before it lets a link go.
    pub link_stood: bool,
    pub switch: SwitchPlan,
    /// `XDG_RUNTIME_DIR`, where the supervisor's control socket goes (a real start only).
    pub socket_dir: Option<PathBuf>,
}

fn short(rev: &str) -> &str {
    &rev[..rev.len().min(10)]
}

// ---- Checks 1-3 -----------------------------------------------------------------------------------

/// Check 1: a real start is never a test (wa_run.rs:1115 `check_not_a_test`).
fn check_not_a_test(vars: &dyn Fn(&str) -> Option<String>) -> Result<()> {
    if let Some(name) = TEST_ENV.iter().find(|n| vars(n).is_some()) {
        bail!(
            "a real run is never a test: threema-run start does not run under `cargo xtask lock` or `cargo xtask ci`, nor inside a test run ({name} is set). Nothing was started."
        );
    }
    Ok(())
}

/// Check 2: the user's go, for EVERY start, before anything is made (wa_run.rs:1125 `check_go`).
fn check_go(a: &Args) -> Result<()> {
    if !a.confirmed {
        bail!(
            "threema-run start contacts Threema's servers. That needs the user's go in this session, for every start: pass --confirmed-by-user only after the user said so. Nothing was started."
        );
    }
    Ok(())
}

/// Check 3: the CPU and memory cap. The app inherits the cgroup of this process, so the cap has to
/// be on `start` itself. A rehearsal, which reaches nothing, may also run in the build container on
/// the agents server, where `container` says so; a real start stays on the PC (the profile, the
/// QR code and his phone are here) and a container does not count for it (wa_run.rs:1140
/// `check_slice`).
pub(super) fn check_slice(
    cgroup: &str,
    container_var: Option<&str>,
    rehearsal: bool,
) -> Result<()> {
    if cgroup.contains("/kourier.slice/") {
        return Ok(());
    }
    if rehearsal {
        if container_var.is_some_and(|c| !c.is_empty()) {
            return Ok(());
        }
        bail!(
            "threema-run rehearse must run inside kourier.slice on the PC (`kcap -m 8G -t 1800 cargo xtask threema-run rehearse`) or in the build container on the agents server (`srv 'kcap -t 5400 cargo xtask threema-run rehearse'`). Nothing was started."
        );
    }
    bail!(
        "threema-run start must run inside kourier.slice (the CPU and memory cap of his rule), on the PC: it never runs on the agents server. Start it as `kcap -t 900 cargo xtask threema-run start ...`. Nothing was started."
    );
}

/// The rehearsal only: the process sees no network interface but `lo`. The rehearsal has no go, so
/// it must be unable to reach anything: it runs in `unshare -rn`, and a start anywhere else is
/// refused before anything is made (wa_run.rs:1742 `check_rehearsal_offline`).
pub(super) fn check_rehearsal_offline(net_dev: &str) -> Result<()> {
    let mut lines = net_dev.lines();
    // `/proc/net/dev` starts with two header lines; a text without them is not read as "alone".
    if !lines
        .next()
        .is_some_and(|l| l.trim_start().starts_with("Inter-"))
        || lines.next().is_none()
    {
        bail!(
            "cannot tell which network interfaces this process sees (/proc/self/net/dev is unreadable), and the rehearsal only runs where nothing can be reached. Nothing was started."
        );
    }
    let seen: Vec<&str> = lines
        .filter_map(|l| l.split(':').next())
        .map(str::trim)
        .filter(|n| !n.is_empty() && *n != "lo")
        .collect();
    if !seen.is_empty() {
        bail!(
            "the rehearsal runs the app with the fake playing Threema and only where nothing can be reached, and this process sees the network interface(s) {}. `cargo xtask threema-run rehearse` runs it in `unshare -rn`. Nothing was started.",
            seen.join(", ")
        );
    }
    Ok(())
}

// ---- Checks 4-5 -----------------------------------------------------------------------------------

/// The worktrees of the repository (`git worktree list --porcelain`; wa_run.rs:1041 `worktrees_of`).
pub(super) fn worktrees_of(git: &dyn Git) -> Result<Vec<PathBuf>> {
    Ok(git
        .run(&["worktree", "list", "--porcelain"])?
        .lines()
        .filter_map(|l| l.strip_prefix("worktree "))
        .map(PathBuf::from)
        .collect())
}

/// Check 4: the place is safe and the disk is encrypted (or the user's word says it need not be).
/// Returns whether it is and the warnings (wa_run.rs:1648 and tg_run/preflight.rs).
fn check_place(
    a: &Args,
    ctx: &Ctx<'_>,
    dir: &ProfileDir,
    warnings: &mut Vec<String>,
) -> Result<bool> {
    let worktrees = worktrees_of(ctx.git)?;
    let places = Places {
        root: ctx.root,
        target: ctx.target,
        worktrees: &worktrees,
    };
    if let Some(why) = unsafe_place(&dir.dir, ctx.vars, &places) {
        bail!(
            "refusing to keep a login in profile {}: {why}. Use --profile <name>.",
            dir.label()
        );
    }
    let found = (ctx.disk)(&dir.dir).is_some();
    if !found {
        if !a.allow_unencrypted {
            bail!(
                "no disk encryption was found below the profile. The profile's database key is a public constant (the development keyring), so on a plain disk the account's keys would lie there readable. Choose a place on the encrypted disk, or pass --allow-unencrypted-disk if the user accepts that."
            );
        }
        warnings.push(
            "--allow-unencrypted-disk: the profile is on a disk without encryption (the user accepted that)"
                .to_owned(),
        );
    }
    Ok(found)
}

/// Check 5: the build's source is exactly main's HEAD with a clean tree (wa_run.rs:1169
/// `check_source`). Returns the commit.
pub(super) fn check_source(git: &dyn Git) -> Result<String> {
    let head = git.run(&["rev-parse", "HEAD"])?.trim().to_owned();
    let main = git
        .run(&["rev-parse", "refs/heads/main"])?
        .trim()
        .to_owned();
    if head != main {
        bail!(
            "HEAD ({}) is not main ({}): a real run's app is built at exactly main's HEAD. Nothing was started.",
            short(&head),
            short(&main)
        );
    }
    let status = git.run(&["status", "--porcelain=v1", "-uall"])?;
    let changed = status.lines().filter(|l| !l.trim().is_empty()).count();
    if changed > 0 {
        bail!(
            "the tree is not clean ({changed} changed or untracked paths): a real run's app is built from a clean main. Nothing was started."
        );
    }
    Ok(head)
}

// ---- Checks 6-8 -----------------------------------------------------------------------------------

/// `YYYY-MM-DD HH:MM:SS UTC` for a time in ms since the epoch (wa_run.rs:227 `utc`). The later
/// chunks of the tool may use it (`super::preflight::utc`).
pub(super) fn utc(ms: u64) -> String {
    let (y, m, d, hh, mm, ss) = civil(ms / 1000);
    format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02}:{ss:02} UTC")
}

/// `YYYYMMDDTHHMMSSZ` for a time in ms since the epoch: a name a directory may have (`backup`).
pub(super) fn utc_compact(ms: u64) -> String {
    let (y, m, d, hh, mm, ss) = civil(ms / 1000);
    format!("{y:04}{m:02}{d:02}T{hh:02}{mm:02}{ss:02}Z")
}

/// Civil date and time of `secs` since the epoch (UTC; Howard Hinnant's algorithm; wa_run.rs:239
/// `civil`).
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

/// Check 6: a run that was halted starts again only with `--after-halt`, the user's word that he
/// knows why. The halt moves to `last_halt` (in memory) so the report can still name it
/// (wa_run.rs:1442 `check_halt`). The words are the halt's kind and time, never a value.
pub(super) fn check_halt(state: &mut State, after_halt: bool) -> Result<()> {
    let halt = state.get("halted").clone();
    let Some(kind) = halt["kind"].as_str() else {
        return Ok(());
    };
    if !after_halt {
        bail!(
            "the last run of this profile was halted ({kind}{}, {}): nothing is retried. Tell the user the kind and read threema-run/app.log; start again only with --after-halt and his word. Nothing was started.",
            halt["code"]
                .as_str()
                .map_or_else(String::new, |code| format!(" {code}")),
            halt["at_ms"]
                .as_u64()
                .map_or_else(|| "at an unknown time".to_owned(), utc),
        );
    }
    state.set("last_halt", halt);
    state.set("halted", Value::Null);
    Ok(())
}

/// Check 7: `--for-link` is for a profile with no account and while no failed link's question is
/// open (C-H2, the S1b rule); without it a start is for a profile that holds an account (there is
/// nothing to connect on one that holds none). A `link_at` that stands is NOT a refusal: the start
/// that finds it opens the question (the launcher), and the profile holds no account then.
pub(super) fn check_for_link(state: &State, for_link: bool, question_open: bool) -> Result<()> {
    if for_link {
        if state.linked() {
            bail!(
                "this profile already has a login: a start --for-link is for a profile without one (the list of test IDs would be written for nothing). Start without --for-link to reconnect, or wipe the profile first. Nothing was started."
            );
        }
        if question_open {
            bail!(
                "a failed link has an open question (the keeper root keeps it): the TEST phone's Linked devices list must be looked at and the question answered before any link starts. Run `threema-run answer` in the user's own terminal, and see the runbook. Nothing was started."
            );
        }
    } else if !state.linked() {
        bail!(
            "this profile holds no login: there is nothing to connect. A first link is a start --for-link. Nothing was started."
        );
    }
    Ok(())
}

/// The blob id of every tracked migration file of this checkout, by `label/relative path`
/// (`git ls-tree -r HEAD`; wa_run.rs:1477 `tracked_migrations`).
pub(super) fn tracked_migrations(git: &dyn Git) -> Result<BTreeMap<String, String>> {
    let mut all = BTreeMap::new();
    for (label, dir) in MIGRATION_DIRS {
        let listing = git.run(&["ls-tree", "-r", "HEAD", "--", dir])?;
        for line in listing.lines() {
            // `<mode> blob <id>\t<path>`
            let Some((meta, path)) = line.split_once('\t') else {
                continue;
            };
            let blob = meta.split_whitespace().nth(2).unwrap_or_default();
            let Some(name) = path.strip_prefix(dir).map(|n| n.trim_start_matches('/')) else {
                continue;
            };
            if name.ends_with(".sql") && !blob.is_empty() {
                all.insert(format!("{label}/{name}"), blob.to_owned());
            }
        }
        if !all.keys().any(|k| k.starts_with(&format!("{label}/"))) {
            bail!("no migration file found below {dir}: cannot tell what this build would run");
        }
    }
    Ok(all)
}

/// Check 8: a migration that ran on this profile is frozen (a change is a new number), and a new
/// one needs a backup and `--allow-schema-change` (wa_run.rs:1517 `check_migrations`).
pub(super) fn check_migrations(
    recorded: &BTreeMap<String, String>,
    current: &BTreeMap<String, String>,
    allow_schema_change: bool,
) -> Result<()> {
    for (name, blob) in recorded {
        match current.get(name) {
            Some(now) if now == blob => {}
            Some(_) => bail!(
                "migration {name} changed after it ran on this profile: a change is a new number. This profile cannot run this build. Nothing was started."
            ),
            None => bail!(
                "migration {name} ran on this profile and is gone from this build. This profile cannot run this build. Nothing was started."
            ),
        }
    }
    let new: Vec<&str> = current
        .keys()
        .filter(|k| !recorded.contains_key(*k))
        .map(String::as_str)
        .collect();
    if !recorded.is_empty() && !new.is_empty() && !allow_schema_change {
        bail!(
            "this build has {} migration file(s) the profile has not run yet ({}). A newer build migrates the profile forward and an older one then refuses it: take a backup first (`threema-run backup`), then start with --allow-schema-change. Nothing was started.",
            new.len(),
            new.join(", ")
        );
    }
    Ok(())
}

// ---- Checks 9-11 ----------------------------------------------------------------------------------

/// Check 9: the clock is synchronized (critic R-L1: the rule `timedatectl` itself uses). The
/// backend closes a connection over 20 minutes of skew (`src/conn/skew.rs`); "Connect Anyway" is
/// after RT1.
pub(super) fn check_clock(clock: Result<Clock>) -> Result<()> {
    let Ok(clock) = clock else {
        bail!(
            "cannot read the kernel's clock state (adjtimex): the start does not go on without knowing the clock is right. Nothing was started."
        );
    };
    if clock.state == libc::TIME_ERROR || clock.status & libc::STA_UNSYNC != 0 {
        bail!(
            "the clock is not synchronized (the kernel says so): the Threema servers close a connection whose clock is over 20 minutes off, and a run on a wrong clock proves nothing. Fix the time (timedatectl) and start again. Nothing was started."
        );
    }
    if clock.maxerror_us >= MAXERROR_LIMIT_US {
        bail!(
            "the kernel's estimate of the clock's error is {} s, not under {} s: the time has not been synchronized for a long while. Let it synchronize (timedatectl) and start again. Nothing was started.",
            clock.maxerror_us / 1_000_000,
            MAXERROR_LIMIT_US / 1_000_000
        );
    }
    Ok(())
}

/// A version as `(major, minor, patch, is_a_release)`: a pre-release (`1.0.0-rc.1`) is below the
/// release of its numbers, build metadata (`+x`) is ignored. `None` for a text that is not one.
fn parse_version(text: &str) -> Option<(u64, u64, u64, bool)> {
    let core = text.split('+').next()?;
    let (numbers, release) = match core.split_once('-') {
        Some((n, _)) => (n, false),
        None => (core, true),
    };
    let mut parts = numbers.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch, release))
}

/// Whether version `found` is at least `needed` (numbers compared as numbers, so 0.103.9 is below
/// 0.103.16). A version this cannot read is below.
pub(super) fn at_least(found: &str, needed: &str) -> bool {
    match (parse_version(found), parse_version(needed)) {
        (Some(found), Some(needed)) => found >= needed,
        _ => false,
    }
}

/// The `(name, version)` of every `[[package]]` of a `Cargo.lock` text, in file order.
pub(super) fn lock_packages(text: &str) -> Vec<(String, String)> {
    let mut all = Vec::new();
    let mut name: Option<String> = None;
    let mut version: Option<String> = None;
    let mut flush = |name: &mut Option<String>, version: &mut Option<String>| {
        if let (Some(n), Some(v)) = (name.take(), version.take()) {
            all.push((n, v));
        }
    };
    for line in text.lines() {
        let line = line.trim();
        if line == "[[package]]" {
            flush(&mut name, &mut version);
        } else if let Some(v) = line
            .strip_prefix("name = \"")
            .and_then(|r| r.strip_suffix('"'))
        {
            name = Some(v.to_owned());
        } else if let Some(v) = line
            .strip_prefix("version = \"")
            .and_then(|r| r.strip_suffix('"'))
        {
            version = Some(v.to_owned());
        }
    }
    flush(&mut name, &mut version);
    all
}

/// Check 10: `Cargo.lock` at HEAD holds main's TLS bump: every `[[package]]` of each name in
/// [`TLS_MINIMUMS`] is at least its version (a lock may hold two versions of one crate, and the old
/// one may be the one Threema's dial links), and each name is there. The refusal names each entry
/// that falls short (critic R-L6).
pub(super) fn check_tls(lock_text: &str) -> Result<()> {
    let packages = lock_packages(lock_text);
    let mut short_of = Vec::new();
    for (name, needed) in TLS_MINIMUMS {
        let found: Vec<&str> = packages
            .iter()
            .filter(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
            .collect();
        if found.is_empty() {
            short_of.push(format!(
                "{name} is not in the lock (needs {needed} or newer)"
            ));
        }
        for version in found {
            if !at_least(version, needed) {
                short_of.push(format!("{name} {version} (needs {needed} or newer)"));
            }
        }
    }
    if !short_of.is_empty() {
        bail!(
            "Cargo.lock at HEAD does not hold the TLS bump Threema's dial needs: {}. The bump is main's (rustls, rustls-webpki, webpki-roots; `webpki-root-certs` is not read: the dial pins through `webpki-roots`); bring it into this tree and commit it before a run. Nothing was started.",
            short_of.join("; ")
        );
    }
    Ok(())
}

/// Whether `text` declares `const <name>:` (any visibility), and, when `words` is given, holds them
/// as a literal on that line or the next (a long declaration wraps after its `=`).
fn declared_with(text: &str, name: &str, words: &str) -> Declared {
    let lines: Vec<&str> = text.lines().collect();
    let needle = format!("const {name}:");
    let Some(at) = lines.iter().position(|l| l.contains(&needle)) else {
        return Declared::Missing;
    };
    let literal = format!("\"{words}\"");
    let here = lines[at].contains(&literal);
    let next = lines.get(at + 1).is_some_and(|l| l.contains(&literal));
    if here || next {
        Declared::Right
    } else {
        Declared::Reworded
    }
}

enum Declared {
    Missing,
    Reworded,
    Right,
}

/// Check 11: every line the tool reads from the app's log stands in HEAD's source, in its words
/// (E6 in the app's, `kourier-threema`'s `conn/lines.rs` for the rest): a renamed or reworded line
/// is a halt rule that never fires, a blind tool, and a blind tool is no run. The sources are HEAD's
/// (`git show`): the tree is clean at HEAD by check 5. The table is data (a parameter): the tool's
/// is [`LINES`].
pub(super) fn check_lines(git: &dyn Git, table: &[LineRow]) -> Result<()> {
    let mut bad: Vec<String> = Vec::new();
    let mut cache: BTreeMap<&str, Option<String>> = BTreeMap::new();
    for row in table {
        let text = cache
            .entry(row.file)
            .or_insert_with(|| git.run(&["show", &format!("HEAD:{}", row.file)]).ok());
        let Some(text) = text else {
            bad.push(format!(
                "{}: {} (the file is not in HEAD)",
                row.file,
                row.name.unwrap_or("a literal")
            ));
            continue;
        };
        let verdict = match row.name {
            Some(name) => declared_with(text, name, row.text),
            None if text.contains(&format!("\"{}\"", row.text)) => Declared::Right,
            None => Declared::Missing,
        };
        match verdict {
            Declared::Right => {}
            Declared::Missing => bad.push(format!(
                "{}: {} (not there)",
                row.file,
                row.name.unwrap_or("a literal")
            )),
            Declared::Reworded => bad.push(format!(
                "{}: {} (its words changed)",
                row.file,
                row.name.unwrap_or("a literal")
            )),
        }
    }
    if !bad.is_empty() {
        bail!(
            "{} line(s) the tool reads from the app's log are not in HEAD's source as the tool expects: {}. A run without them is blind (a halt rule that never fires). Nothing was started.",
            bad.len(),
            bad.join("; ")
        );
    }
    Ok(())
}

// ---- Check 12 -------------------------------------------------------------------------------------

/// The app's per-profile lock (`<profile>/kourier/data/lock`, `kourier-app/src/paths.rs`
/// `LOCK_FILE`): held while a Kourier runs on the profile.
pub(super) fn app_lock_file(dir: &ProfileDir) -> PathBuf {
    dir.app_home().join("data").join("lock")
}

/// The names of the profiles in a keeper root: its directories (not links) with a name a profile
/// may have, in order (the list's file, `backups`, the keeper lock and a stray file are none).
pub(super) fn profiles_of(keeper: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(keeper)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| valid_profile_name(n))
        .collect();
    names.sort();
    names
}

/// Check 12: the keeper root. The keeper lock is held only while a list of test IDs may stand (a
/// link start holds it): another run holds it, so the start waits. No other Kourier and no other
/// supervisor runs on the profiles that matter: on EVERY profile of the keeper root for a link
/// start (the list is read by the app of any profile, `threema-app.md` THE ALLOW-LIST FILE), on this
/// one otherwise. Returns whether a leftover list stands (no run holds the keeper lock: it is not
/// a live run's).
pub(super) fn check_keeper(a: &Args, ctx: &Ctx<'_>, dir: &ProfileDir) -> Result<bool> {
    let held = |path: &Path| -> Result<bool> {
        (ctx.lock)(path).map(|l| l == Lock::Held).map_err(|_| {
            anyhow::anyhow!(
                "cannot tell whether a run holds a lock of the keeper root. Nothing was started."
            )
        })
    };
    let keeper_lock = dir.keeper_lock_file();
    if held(&keeper_lock)? {
        bail!(
            "another run holds the keeper lock of the keeper root: a list of test IDs may stand for a link right now. Ask again when that run has taken it. Nothing was started."
        );
    }
    let mut profiles: Vec<ProfileDir> = if a.for_link {
        profiles_of(dir.keeper())
            .into_iter()
            .map(|name| ProfileDir {
                dir: dir.keeper().join(name),
                rehearsal: dir.rehearsal,
            })
            .filter(|p| p.dir != dir.dir)
            .collect()
    } else {
        Vec::new()
    };
    profiles.push(dir.clone());
    for profile in profiles {
        let name = profile.label();
        if held(&app_lock_file(&profile))? {
            bail!(
                "a Kourier is running on profile {name}{}. Stop it first (`threema-run stop`, or close the window). Nothing was started.",
                if a.for_link {
                    " (a link start waits until no Kourier runs on any profile of the keeper root: the list of test IDs would be read by it)"
                } else {
                    ""
                }
            );
        }
        if held(&profile.lock_file())? {
            bail!(
                "another threema-run supervisor holds profile {name}. `threema-run state` shows it, `threema-run stop` ends it. Nothing was started."
            );
        }
    }
    Ok(std::fs::symlink_metadata(dir.allow_file()).is_ok())
}

// ---- Check 13 -------------------------------------------------------------------------------------

/// The `networks-on` entry of a `kourier.toml`: the names, and the lines it spans (an array may
/// run over several lines), counted from 0, last inclusive.
struct Entry {
    names: Vec<String>,
    first: usize,
    last: usize,
}

/// Reads the switches of a `kourier.toml`: the top-level `networks-on` entry (an unreadable one, a
/// second one, or text after its `]` that is no comment is an error: the tool never rewrites what
/// it does not understand) and whether a `pending-link` line stands anywhere. Keys are bare, as the
/// app writes them (`prefs.rs`); a `[table]` header ends the top level.
fn scan_config(text: &str) -> Result<(Option<Entry>, bool)> {
    let unreadable = || {
        anyhow::anyhow!(
            "the profile's kourier.toml has a networks-on entry this tool does not read (a list of names on one or several lines is what it knows): fix it or wipe the profile. Nothing was started."
        )
    };
    let lines: Vec<&str> = text.lines().collect();
    let key_of = |line: &str, key: &str| -> Option<String> {
        line.strip_prefix(key)
            .map(str::trim_start)
            .and_then(|r| r.strip_prefix('='))
            .map(str::to_owned)
    };
    let mut entry: Option<Entry> = None;
    let mut pending = false;
    let mut top = true;
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i].trim();
        if line.starts_with('[') {
            top = false;
        }
        if key_of(line, "pending-link").is_some() {
            pending = true;
        }
        if top && let Some(value) = key_of(line, "networks-on") {
            if entry.is_some() {
                return Err(unreadable());
            }
            let first = i;
            let mut body = value.trim().to_owned();
            while !body.contains(']') {
                i += 1;
                let Some(next) = lines.get(i) else {
                    return Err(unreadable());
                };
                body.push(' ');
                body.push_str(next.trim());
            }
            let Some(inner) = body.strip_prefix('[') else {
                return Err(unreadable());
            };
            let (list, rest) = inner.split_once(']').ok_or_else(unreadable)?;
            let rest = rest.trim();
            if !(rest.is_empty() || rest.starts_with('#')) {
                return Err(unreadable());
            }
            entry = Some(Entry {
                names: list
                    .split(',')
                    .map(|n| n.trim().trim_matches(['"', '\'']).to_owned())
                    .filter(|n| !n.is_empty())
                    .collect(),
                first,
                last: i,
            });
        }
        i += 1;
    }
    Ok((entry, pending))
}

/// The text of a profile's `kourier.toml` with the switch on (the network's name) or off (no name)
/// and nothing else changed: the `networks-on` entry is replaced (all the lines it spans), a missing
/// one is put in front of the file (a top-level key must come before any `[table]`), a missing file
/// is just that line. The start writes the ON text; the run's end writes the OFF text of the
/// file as it is THEN (the app may have rewritten it meanwhile).
pub(super) fn switch_text(existing: Option<&str>, on: bool) -> Result<String> {
    let entry_line = if on {
        format!("networks-on = [\"{NETWORK_NAME}\"]")
    } else {
        "networks-on = []".to_owned()
    };
    let Some(text) = existing else {
        return Ok(format!("{entry_line}\n"));
    };
    let (entry, _) = scan_config(text)?;
    let lines: Vec<&str> = text.lines().collect();
    let mut out: Vec<&str> = Vec::with_capacity(lines.len() + 1);
    match entry {
        Some(e) => {
            out.extend_from_slice(&lines[..e.first]);
            out.push(&entry_line);
            out.extend_from_slice(&lines[e.last + 1..]);
        }
        None => {
            out.push(&entry_line);
            out.extend_from_slice(&lines);
        }
    }
    Ok(format!("{}\n", out.join("\n")))
}

/// Check 13: the switch of the run planned, nothing written. `networks-on` may name nothing or
/// the network (a hand-edited profile could register another network too: refused), and a
/// `pending-link` line is refused (the app would open a link page by itself, outside `step link`;
/// wa_run.rs:1580 `plan_switch`, tg_run `check_switches`).
pub(super) fn plan_switch(existing: Option<&str>) -> Result<SwitchPlan> {
    let (entry, pending) = match existing {
        Some(text) => scan_config(text)?,
        None => (None, false),
    };
    if pending {
        bail!(
            "the profile asks to open a link page by itself (a `pending-link` line in kourier.toml). Open Kourier once on it without the tool, or remove that line. Nothing was started."
        );
    }
    if let Some(entry) = &entry {
        let others = entry
            .names
            .iter()
            .filter(|n| !n.eq_ignore_ascii_case(NETWORK_NAME))
            .count();
        if others > 0 {
            bail!(
                "the profile's networks-on names another network ({others} name(s) besides {NETWORK_NAME}): fix kourier.toml or wipe the profile. Nothing was started."
            );
        }
    }
    Ok(SwitchPlan {
        existing: existing.map(str::to_owned),
        on_text: switch_text(existing, true)?,
        found_on: entry.is_some_and(|e| !e.names.is_empty()),
    })
}

// ---- Check 14 -------------------------------------------------------------------------------------

/// Check 14: the supervisor's control socket lives in `XDG_RUNTIME_DIR`, which must be set,
/// absolute, a directory of this user's with mode 0700, and short enough for the socket's name to
/// fit (the place never depends on the checkout's length, C7). Returns the directory (the launcher
/// makes the socket's name).
pub(super) fn check_socket_place(
    vars: &dyn Fn(&str) -> Option<String>,
    uid: u32,
) -> Result<PathBuf> {
    let Some(dir) = vars("XDG_RUNTIME_DIR").filter(|d| !d.is_empty()) else {
        bail!(
            "XDG_RUNTIME_DIR is not set: the supervisor's control socket lives there (a short place that does not depend on the checkout). Start it from the user's own session. Nothing was started."
        );
    };
    let dir = PathBuf::from(dir);
    if !dir.is_absolute() {
        bail!("XDG_RUNTIME_DIR is not an absolute path. Nothing was started.");
    }
    let meta = match std::fs::symlink_metadata(&dir) {
        Ok(meta) => meta,
        Err(_) => bail!("XDG_RUNTIME_DIR cannot be looked at. Nothing was started."),
    };
    if !meta.is_dir() || meta.uid() != uid || meta.mode() & 0o7777 != 0o700 {
        bail!(
            "XDG_RUNTIME_DIR is not your private directory (a directory of yours with mode 0700, not a link): the control socket would not be safe there. Nothing was started."
        );
    }
    let len = dir.as_os_str().len();
    if len + SOCKET_NAME_LEN > SOCKET_PATH_MAX {
        bail!(
            "XDG_RUNTIME_DIR is {len} bytes long: the control socket's name would not fit a socket path ({SOCKET_PATH_MAX} bytes). Nothing was started."
        );
    }
    Ok(dir)
}

// ---- The real context -----------------------------------------------------------------------------

/// Runs `f` over the context the preflight reads from this machine (tg_run/mod.rs `with_real_ctx`):
/// the checkout's git, `/proc/self/cgroup` and `/proc/self/net/dev`, the environment, the disk's
/// encryption, the test lock, the real `adjtimex`, the real lock probes and the tool's own table of
/// lines. `start` runs the preflight over it; nothing in it makes anything.
pub(super) fn with_real_ctx<T>(f: impl FnOnce(&Ctx<'_>) -> Result<T>) -> Result<T> {
    let root = crate::root();
    let target = crate::target_dir();
    let git = RealGit { root: &root };
    let cgroup = std::fs::read_to_string("/proc/self/cgroup").unwrap_or_default();
    let net_dev = std::fs::read_to_string("/proc/self/net/dev").unwrap_or_default();
    let now = || {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or(0)
    };
    let lock = |path: &Path| probe_lock(path, PROBE_SETTLE);
    // SAFETY: getuid has no preconditions and cannot fail.
    let uid = unsafe { libc::getuid() };
    let ctx = Ctx {
        vars: &super::var,
        root: &root,
        target: &target,
        git: &git,
        cgroup: &cgroup,
        disk: &super::profile::disk_encryption,
        test_lock_busy: &test_lock_busy,
        net_dev: &net_dev,
        uid,
        now_ms: &now,
        clock: &read_clock,
        lock: &lock,
        lines: LINES,
    };
    f(&ctx)
}

// ---- The whole ------------------------------------------------------------------------------------

/// Every check of `start`, in order (the table at the top). Nothing is made, started or written
/// here but the answer.
pub(super) fn preflight(a: &Args, ctx: &Ctx<'_>) -> Result<Cleared> {
    let rehearsal = a.place.rehearsal_place.is_some();
    // 1, 2
    if !rehearsal {
        check_not_a_test(ctx.vars)?;
        check_go(a)?;
    }
    // 3
    check_slice(ctx.cgroup, (ctx.vars)("container").as_deref(), rehearsal)?;
    if rehearsal {
        check_rehearsal_offline(ctx.net_dev)?;
    }
    // 4
    let dir = a.place.resolve(ctx.vars, ctx.target)?;
    let mut warnings = Vec::new();
    let encrypted = if rehearsal {
        None
    } else {
        Some(check_place(a, ctx, &dir, &mut warnings)?)
    };
    // 5
    let head = if rehearsal {
        None
    } else {
        Some(check_source(ctx.git)?)
    };
    // 6
    let mut state = match State::load(&dir)? {
        Some(state) => state,
        None => {
            if dir.app_home().exists() {
                bail!(
                    "profile {} holds Kourier data but no threema-run state file: it is not a profile of this tool, and it is not adopted",
                    dir.label()
                );
            }
            State::new((ctx.now_ms)())
        }
    };
    check_halt(&mut state, a.after_halt)?;
    // 7
    let question_open = std::fs::symlink_metadata(dir.open_question_file()).is_ok();
    check_for_link(&state, a.for_link, question_open)?;
    if state.link_stood() {
        warnings.push(
            "a login of this profile reached linking and its end was not seen: a device may be registered on the TEST phone; the start asks about it before a link goes on"
                .to_owned(),
        );
    }
    // 8
    let migrations = if rehearsal {
        None
    } else {
        let current = tracked_migrations(ctx.git)?;
        check_migrations(&state.migrations(), &current, a.allow_schema_change)?;
        Some(current)
    };
    // 9, 10, 11
    if !rehearsal {
        check_clock((ctx.clock)())?;
        check_tls(&ctx.git.run(&["show", "HEAD:Cargo.lock"])?)?;
        check_lines(ctx.git, ctx.lines)?;
    }
    // 12
    let leftover_list = check_keeper(a, ctx, &dir)?;
    if leftover_list {
        warnings.push(format!(
            "a list of test IDs ({ALLOW_FILE}) was left by a run that did not end cleanly: the start removes it"
        ));
    }
    // 13
    let existing = match std::fs::read_to_string(dir.config_file()) {
        Ok(text) => Some(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).context("read the profile's kourier.toml"),
    };
    let switch = plan_switch(existing.as_deref())?;
    if switch.found_on {
        warnings.push(
            "the profile's switch was on with no run alive: the last run did not end cleanly (the end of a run switches Threema off)"
                .to_owned(),
        );
    }
    // 14
    let socket_dir = if rehearsal {
        None
    } else {
        Some(check_socket_place(ctx.vars, ctx.uid)?)
    };
    if !rehearsal && (ctx.test_lock_busy)() {
        warnings.push(
            "a test run (cargo xtask ci) holds the test lock: the machine is busy and the QR code's window may be missed"
                .to_owned(),
        );
    }
    let link_stood = state.link_stood();
    Ok(Cleared {
        dir,
        state,
        warnings,
        encrypted,
        head,
        migrations,
        leftover_list,
        link_stood,
        switch,
        socket_dir,
    })
}
