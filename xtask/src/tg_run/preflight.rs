//! The preflight of `connect` (brief §2.4 cut to R0a, in this order): a list of checks, each its own
//! function that takes what it reads as an argument (the environment, the repository root, a [`Git`]
//! runner, the cgroup text, the library check), so a test can make each refusal on its own and assert
//! ITS text, whatever test run it is itself part of. Nothing is made, started or written here but the
//! answer.
//!
//! Real run: not a test, the user's go, `--env test`, `kourier.slice`, no TDLib or API variable in the
//! tool's environment, a safe and encrypted place, a profile this tool made (or none), HEAD == main
//! with a clean tree, the address list VERIFIED, the developer's API file (its metadata only), the
//! checked TDLib library, `networks-on` exactly Telegram, and a warning for a busy test lock.
//! A rehearsal place (`--rehearsal-place`, inside `unshare -rn`, no network) keeps the cap, the
//! environment, the env binding and `networks-on`, and skips the rest: it has no go (the order's
//! confirmation is the flag alone), no API file, no library, no list and no clean main.

use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use super::profile::{Places, ProfileDir, State, keeper_root, read_switches, unsafe_place};
use super::stage::CheckedLibrary;
use super::{API_FILE, Args, CONNECT_COMMAND, KEEPER_DIR};
use crate::lock;

/// The one file that names Telegram's address blocks, relative to the repository root.
pub(super) const RANGES_RS: &str = "crates/kourier-telegram/src/gate/ranges.rs";
/// The two blocks of it the list hash covers: the line each starts with.
const LIST_BLOCKS: [&str; 2] = [
    "pub(crate) const TELEGRAM_V4",
    "pub(crate) const TELEGRAM_V6",
];
/// The line that ends a block.
const BLOCK_END: &str = "];";
/// Variables that mean "this is a test run" (a real run refuses to start inside one). Exact names:
/// `NEXTEST_TEST_THREADS` is kcap's and says nothing (wa_run.rs:122 `TEST_ENV`).
pub(super) const TEST_ENV: &[&str] = &[
    lock::HELD_ENV,
    "NEXTEST",
    "NEXTEST_RUN_ID",
    "NEXTEST_EXECUTION_MODE",
];
/// The library the helper would open, taken from the environment in a debug build: a tool that finds
/// it set runs a library nobody pinned, so it refuses (P-18).
const TDJSON_ENV: &str = "KOURIER_TDJSON";
/// The developer's API file may be this long at most (the app's own bound).
const API_FILE_MAX: u64 = 256;

/// A read-only runner of git commands in the repository (a seam: the tests fake it or point it at a
/// repository of their own; wa_run.rs:905 `Git`).
pub(super) trait Git {
    fn run(&self, args: &[&str]) -> Result<String>;
}

/// `git -C <root> …`, with no optional locks, so `status` does not write the index (wa_run.rs:910
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
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
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
    /// The uid the API file and its directory must belong to.
    pub uid: u32,
    /// The checked TDLib library and its sum (`tdlib::checked_library` in the tool).
    pub library: &'a dyn Fn() -> Result<CheckedLibrary>,
}

/// What the preflight found.
#[derive(Debug)]
pub(super) struct Cleared {
    pub dir: ProfileDir,
    /// The profile's state: the file's, or a fresh one bound to the requested servers.
    pub state: State,
    /// Lines to print before anything starts.
    pub warnings: Vec<String>,
    /// Whether the disk below the profile is encrypted; `None` in a rehearsal, which looks at no disk.
    pub encrypted: Option<bool>,
    /// main's HEAD the build is made from (a real run only).
    pub head: Option<String>,
    /// Whether the address list carries a verification (false with `--allow-unverified-ranges`;
    /// `None` in a rehearsal: it checks no list).
    pub ranges_verified: Option<bool>,
    /// The checked TDLib library and the sum it was checked with (a real run only).
    pub library: Option<CheckedLibrary>,
}

fn short(rev: &str) -> &str {
    &rev[..rev.len().min(10)]
}

/// Check 1: a real run is never a test (wa_run.rs:998 `check_not_a_test`).
fn check_not_a_test(vars: &dyn Fn(&str) -> Option<String>) -> Result<()> {
    if let Some(name) = TEST_ENV.iter().find(|n| vars(n).is_some()) {
        bail!(
            "a real run is never a test: tg-run connect does not run under `cargo xtask lock` or `cargo xtask ci`, nor inside a test run ({name} is set). Nothing was started."
        );
    }
    Ok(())
}

/// Check 2: the user's go, before anything is made (wa_run.rs:1008 `check_go`).
fn check_go(a: &Args) -> Result<()> {
    if !a.confirmed {
        bail!(
            "tg-run connect contacts Telegram's servers. That needs the user's go in this session: pass --confirmed-by-user only after the user said so. Nothing was started."
        );
    }
    Ok(())
}

/// Check 3: the servers. Only `test` (D-4 Q1); the value is never echoed.
fn check_env(env: Option<&str>) -> Result<()> {
    match env {
        Some("test") => Ok(()),
        Some("production") => {
            bail!("production comes with R2: tg-run takes --env test only. Nothing was started.")
        }
        Some(_) => bail!("--env must be test (production comes with R2). Nothing was started."),
        None => bail!("--env is required: pass --env test. Nothing was started."),
    }
}

/// Check 4: the CPU and memory cap. The app inherits the cgroup of this process, so the cap has to
/// be on the tool itself. A rehearsal, which reaches nothing, may also run in the build container
/// on the agents server (his "Execution only", 2026-10-27): there the container's own limits are the
/// cap (the server's `kcap` only times out), and `container` (`container_var`), which a container
/// manager sets for everything it starts, says so. A real run stays on the PC and in
/// `kourier.slice`: a container does not count for it (wa_run.rs:1023 `check_slice`).
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
            "tg-run rehearse must run inside kourier.slice on the PC (`kcap -m 8G -t 1800 cargo xtask tg-run rehearse`) or in the build container on the agents server (`srv 'kcap -t 5400 cargo xtask tg-run rehearse'`). Nothing was started."
        );
    }
    bail!(
        "tg-run connect must run inside kourier.slice (the CPU and memory cap of his rule): start it as `{CONNECT_COMMAND}` (with the options you gave). Nothing was started."
    );
}

/// Check (the rehearsal only): the process sees no network interface but `lo`. The rehearsal skips
/// the user's go, so it must be unable to reach anything: it runs in `unshare -rn`, and a start
/// anywhere else is refused before anything is made (wa_run.rs:1400 `check_rehearsal_offline`).
fn check_rehearsal_offline(net_dev: &str) -> Result<()> {
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
            "the rehearsal runs the app with the fake playing Telegram and only where nothing can be reached, and this process sees the network interface(s) {}. `cargo xtask tg-run rehearse` runs it in `unshare -rn`. Nothing was started.",
            seen.join(", ")
        );
    }
    Ok(())
}

/// Check 5: the tool's environment holds no library path and no API pair (critic 22, P-18). They
/// are REFUSED, not stripped: their presence means a confused setup, and the app takes the library
/// from the stamped copy and the pair from the developer's file alone. Only the names are said.
fn check_environment(vars: &dyn Fn(&str) -> Option<String>) -> Result<()> {
    if vars(TDJSON_ENV).is_some() {
        bail!(
            "{TDJSON_ENV} is set in the tool's environment: the run uses the pinned library it stages itself, never one named by a variable. Unset it. Nothing was started."
        );
    }
    if let Some(name) = crate::TELEGRAM_API_VARS.iter().find(|n| vars(n).is_some()) {
        bail!(
            "{name} is set in the tool's environment: the API pair comes from the developer's file alone. Unset it. Nothing was started."
        );
    }
    Ok(())
}

/// The worktrees of the repository (`git worktree list --porcelain`; wa_run.rs:1041 `worktrees_of`).
fn worktrees_of(git: &dyn Git) -> Result<Vec<PathBuf>> {
    Ok(git
        .run(&["worktree", "list", "--porcelain"])?
        .lines()
        .filter_map(|l| l.strip_prefix("worktree "))
        .map(PathBuf::from)
        .collect())
}

/// The build's source is exactly main's HEAD with a clean tree (wa_run.rs:1052 `check_source`).
/// Returns the commit.
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

// ---- The address list ---------------------------------------------------------------------------

/// The sentence every refusal of the list ends in.
const LIST_REASON: &str = "R0 needs Telegram's published list fetched with the user's go first, and the VERIFIED line written from `cargo xtask tg-run ranges`.";

/// THE bytes the list hash covers, in one place: for each of the two blocks, in this order
/// (`TELEGRAM_V4`, then `TELEGRAM_V6`), the lines from the one that starts the block to the first
/// line that is `];`, each line followed by `\n` (line ends and trailing white space of a line as
/// `str::lines` leaves them; the doc comments above a block, the rest of the file and the VERIFIED
/// line are not covered, so writing the marker or a comment does not change the hash; an edit of a
/// block does). A missing or doubled block, or one without its end, is an error.
pub(super) fn list_bytes(text: &str) -> Result<Vec<u8>> {
    let mut out = String::new();
    for start in LIST_BLOCKS {
        let lines: Vec<&str> = text.lines().collect();
        let mut starts = lines
            .iter()
            .enumerate()
            .filter(|(_, l)| l.starts_with(start));
        let Some((at, _)) = starts.next() else {
            bail!("the address list has no block {start}");
        };
        if starts.next().is_some() {
            bail!("the address list has the block {start} twice");
        }
        let end = lines[at..]
            .iter()
            .position(|l| l.trim_end() == BLOCK_END)
            .with_context(|| format!("the block {start} of the address list has no end"))?;
        for line in &lines[at..=at + end] {
            out.push_str(line);
            out.push('\n');
        }
    }
    Ok(out.into_bytes())
}

/// `sha256` of `bytes` as 64 lower-case hex digits, by `sha256sum` on stdin (wa_run's twin in
/// tdlib.rs:704 `sha256_bytes`; the tool adds no crate).
pub(super) fn sha256_bytes(bytes: &[u8]) -> Result<String> {
    use std::io::Write as _;
    let mut child = Command::new("sha256sum")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .context("spawn sha256sum")?;
    child
        .stdin
        .take()
        .context("sha256sum stdin")?
        .write_all(bytes)?;
    let out = child.wait_with_output()?;
    if !out.status.success() {
        bail!("sha256sum failed");
    }
    let text = String::from_utf8_lossy(&out.stdout);
    Ok(text
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_owned())
}

/// The list hash of a `ranges.rs` text: what the VERIFIED line's `list-sha256:` must equal, and what
/// `tg-run ranges` prints for the working tree.
pub(super) fn list_sha256(text: &str) -> Result<String> {
    sha256_bytes(&list_bytes(text)?)
}

fn is_hex64(text: &str) -> bool {
    text.len() == 64
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// `yyyy-mm-dd` with a month 01-12 and a day 01-31.
fn is_date(text: &str) -> bool {
    let b = text.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return false;
    }
    let digits = |s: &[u8]| s.iter().all(u8::is_ascii_digit);
    if !(digits(&b[..4]) && digits(&b[5..7]) && digits(&b[8..])) {
        return false;
    }
    let month: u32 = text[5..7].parse().unwrap_or(0);
    let day: u32 = text[8..].parse().unwrap_or(0);
    (1..=12).contains(&month) && (1..=31).contains(&day)
}

/// Check: the address list of the COMMITTED `ranges.rs` (`text`) is verified. It holds no
/// `UNVERIFIED` word and exactly one line `//! VERIFIED <yyyy-mm-dd> sha256:<64 hex>
/// list-sha256:<64 hex> <source>`, whose `list-sha256` equals the tool's own hash of the two blocks
/// ([`list_bytes`]): a list edited after it was verified is refused (critic 12). The `sha256:` is the
/// hash of what was fetched; the tool cannot check it, he wrote it from the fetch.
pub(super) fn check_ranges(text: &str) -> Result<()> {
    if text.contains("UNVERIFIED") {
        bail!("the address list is marked UNVERIFIED. {LIST_REASON} Nothing was started.");
    }
    let lines: Vec<&str> = text
        .lines()
        .filter(|l| l.starts_with("//! VERIFIED"))
        .collect();
    let [line] = lines.as_slice() else {
        bail!(
            "the address list has {} VERIFIED line(s), not one. {LIST_REASON} Nothing was started.",
            lines.len()
        );
    };
    let mut words = line["//! VERIFIED".len()..].split_whitespace();
    let (date, fetched, listed) = (words.next(), words.next(), words.next());
    let source: Vec<&str> = words.collect();
    let shape_ok = date.is_some_and(is_date)
        && fetched
            .and_then(|w| w.strip_prefix("sha256:"))
            .is_some_and(is_hex64)
        && listed
            .and_then(|w| w.strip_prefix("list-sha256:"))
            .is_some_and(is_hex64)
        && !source.is_empty();
    if !shape_ok {
        bail!(
            "the VERIFIED line of the address list is not `//! VERIFIED <yyyy-mm-dd> sha256:<64 hex> list-sha256:<64 hex> <source>`. {LIST_REASON} Nothing was started."
        );
    }
    let claimed = listed
        .and_then(|w| w.strip_prefix("list-sha256:"))
        .unwrap_or_default();
    if list_sha256(text)? != claimed {
        bail!(
            "the address list was edited after it was verified (its list-sha256 no longer matches the two blocks). {LIST_REASON} Nothing was started."
        );
    }
    Ok(())
}

// ---- The developer's API file -------------------------------------------------------------------

/// The recipe for a file that is not there: a private file made EMPTY by the shell and filled in an
/// editor, so the values are never on a command line or in the shell's history.
const API_RECIPE: &str = "write it yourself, with nothing in the shell history: `install -d -m 0700 \"${XDG_DATA_HOME:-$HOME/.local/share}/kourier-tg-run\" && install -m 0600 /dev/null \"${XDG_DATA_HOME:-$HOME/.local/share}/kourier-tg-run/telegram-api\"`, then open that file in an editor and write the two lines id=<digits> and hash=<32 lower-case hex digits>";

/// Check: the developer's API file is HIS and private. Its METADATA only (`lstat`): the file is
/// never opened, so a FIFO cannot hang the tool and no value is read; a regular file, this user's,
/// mode 0600, one name, at most 256 bytes and not empty, in a directory that is this user's and 0700.
/// The app does the reading (and checks the same on the open descriptor). `keeper` is the keeper
/// root; the directory and the file must belong to `dir_uid` and `file_uid` (both the user's in the
/// tool; a test passes another one, because the owner arms cannot be reached by a plain user
/// otherwise).
pub(super) fn check_api_file(keeper: &Path, dir_uid: u32, file_uid: u32) -> Result<()> {
    let missing = || {
        anyhow::anyhow!(
            "your Telegram API file ({KEEPER_DIR}/{API_FILE}) is missing: {API_RECIPE}. Nothing was started."
        )
    };
    let dir = match std::fs::symlink_metadata(keeper) {
        Ok(dir) => dir,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(missing()),
        Err(_) => bail!("cannot look at the {KEEPER_DIR} directory. Nothing was started."),
    };
    if !dir.is_dir() || dir.uid() != dir_uid || dir.mode() & 0o7777 != 0o700 {
        bail!(
            "the {KEEPER_DIR} directory is not your private directory (a directory of yours with mode 0700). Nothing was started."
        );
    }
    let meta = match std::fs::symlink_metadata(keeper.join(API_FILE)) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(missing()),
        Err(_) => bail!("cannot look at the Telegram API file. Nothing was started."),
    };
    let kind = meta.file_type();
    if !kind.is_file() {
        let what = if kind.is_symlink() {
            "a link"
        } else if kind.is_dir() {
            "a directory"
        } else if kind.is_fifo() {
            "a pipe"
        } else {
            "not a regular file"
        };
        bail!("the Telegram API file is {what}, not a regular file. Nothing was started.");
    }
    if meta.uid() != file_uid {
        bail!("the Telegram API file is not owned by you. Nothing was started.");
    }
    if meta.mode() & 0o7777 != 0o600 {
        bail!(
            "the Telegram API file is not private: its mode must be 0600 (chmod 0600). Nothing was started."
        );
    }
    if meta.nlink() != 1 {
        bail!("the Telegram API file has another name (a hard link). Nothing was started.");
    }
    if meta.len() == 0 {
        bail!(
            "the Telegram API file is empty: open it in an editor and write the two lines id=<digits> and hash=<32 lower-case hex digits>. Nothing was started."
        );
    }
    if meta.len() > API_FILE_MAX {
        bail!(
            "the Telegram API file is longer than {API_FILE_MAX} bytes: it is two short lines. Nothing was started."
        );
    }
    Ok(())
}

// ---- The profile's switches ----------------------------------------------------------------------

/// Check (critic 25): when the profile's `kourier.toml` exists, its `networks-on` names exactly
/// Telegram (a hand-edited profile could register another network too), and a `pending-link` is
/// Telegram's. The writer of the switch file keeps every other key (the next chunk).
pub(super) fn check_switches(config: Option<&str>) -> Result<()> {
    let Some(text) = config else {
        return Ok(());
    };
    let (names, pending) = read_switches(text);
    if let Some(names) = names
        && names.as_slice() != ["Telegram"]
    {
        bail!(
            "the profile's networks-on must name Telegram and nothing else (it names {} network(s)): fix kourier.toml or wipe the profile. Nothing was started.",
            names.len()
        );
    }
    if let Some(pending) = pending
        && pending != "Telegram"
    {
        bail!(
            "the profile asks to open another network's link page by itself (a `pending-link` line that is not Telegram's): fix kourier.toml or wipe the profile. Nothing was started."
        );
    }
    Ok(())
}

/// Every check of `connect`, in order. Nothing is made, started or written here but the answer.
pub(super) fn preflight(a: &Args, ctx: &Ctx<'_>) -> Result<Cleared> {
    let rehearsal = a.place.rehearsal_place.is_some();
    if !rehearsal {
        check_not_a_test(ctx.vars)?;
        check_go(a)?;
    }
    check_env(a.env.as_deref())?;
    check_slice(ctx.cgroup, (ctx.vars)("container").as_deref(), rehearsal)?;
    if rehearsal {
        check_rehearsal_offline(ctx.net_dev)?;
    }
    check_environment(ctx.vars)?;
    let dir = a.place.resolve(ctx.vars, ctx.target)?;
    let mut warnings = Vec::new();
    let mut encrypted = None;
    if !rehearsal {
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
        encrypted = Some(found);
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
    }
    let state = match State::load(&dir)? {
        Some(state) => state,
        None => {
            if dir.app_home().exists() {
                bail!(
                    "profile {} holds Kourier data but no tg-run state file: it is not a profile of this tool, and it is not adopted",
                    dir.label()
                );
            }
            State::new("test")
        }
    };
    if state.env() != a.env.as_deref() {
        bail!(
            "profile {} is bound to the servers it was first started for, and this start asks for others: use a new profile name (a profile never changes servers). Nothing was started.",
            dir.label()
        );
    }
    let (mut head, mut ranges_verified, mut library) = (None, None, None);
    if !rehearsal {
        head = Some(check_source(ctx.git)?);
        if a.allow_unverified_ranges {
            ranges_verified = Some(false);
            warnings.push(
                "--allow-unverified-ranges: the address list is NOT verified against Telegram's published one (the user's word); a non-zero 'rule' count in the verdict means the list is stale"
                    .to_owned(),
            );
        } else {
            check_ranges(&ctx.git.run(&["show", &format!("HEAD:{RANGES_RS}")])?)?;
            ranges_verified = Some(true);
        }
        let keeper = keeper_root(ctx.vars)?;
        check_api_file(&keeper, ctx.uid, ctx.uid)?;
        library = Some((ctx.library)()?);
    }
    let existing = match std::fs::read_to_string(dir.config_file()) {
        Ok(text) => Some(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).context("read the profile's kourier.toml"),
    };
    check_switches(existing.as_deref())?;
    if !rehearsal && (ctx.test_lock_busy)() {
        warnings.push(
            "a test run (cargo xtask ci) holds the test lock: the machine is busy and the QR code's windows may be missed"
                .to_owned(),
        );
    }
    Ok(Cleared {
        dir,
        state,
        warnings,
        encrypted,
        head,
        ranges_verified,
        library,
    })
}
