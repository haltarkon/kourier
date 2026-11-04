//! `cargo xtask tg-run <command>`: the run tool of the Telegram backend in its R0a cut (TR; the
//! runbook is `docs/telegram-run.md`). One confirmed start of the app on a kept profile, ONE login
//! QR code that nobody scans (the app runs offscreen, no account can result), the gate's counts
//! judged; a rehearsal of all of it against the fake that plays Telegram; `wipe`; `ranges`.
//!
//! ```text
//! cargo xtask tg-run connect --confirmed-by-user --env test [--profile <name>]   # the only command that contacts Telegram
//! cargo xtask tg-run rehearse                                                     # the whole tool against the fake, no network
//! cargo xtask tg-run wipe [--profile <name>] --yes-delete-the-login
//! cargo xtask tg-run ranges                                                       # the list hash for the VERIFIED line
//! ```
//!
//! It is `wa_run.rs`'s sibling for Telegram, written beside it and not on top of it: what it needs of
//! that tool is COPIED (each copy names its source as `(wa_run.rs:<line> <fn>)`), so `wa_run.rs` and
//! `smoke.rs` stay untouched and a change to one run tool cannot move another. `.claude/rules/dev-runs.md`
//! lists the three copies, so a security fix lands in all of them until a shared extraction.
//!
//! # The rules this tool keeps
//!
//! * Nothing contacts Telegram except `connect --confirmed-by-user`, and that only after the user's go
//!   in that session. A real run is never a test; only the rehearsal (`--rehearsal-place`, a
//!   throwaway place below `target/tgr/`, the fake, `unshare -rn`) is exempt, and it never touches a
//!   kept profile.
//! * A kept profile lives only in `${XDG_DATA_HOME:-~/.local/share}/kourier-tg-run/<name>` (0700),
//!   outside the repository, its worktrees and its build directories, and apart from every other
//!   tool's data. The developer's API file beside it is HIS: the tool checks its metadata and never
//!   opens, copies, prints or deletes it.
//! * What the tool prints or writes is meant for someone who must not see names, numbers or texts:
//!   counts, words and times only. A command line takes no secret: the parser refuses any flag that
//!   could carry one, without echoing it.
//!
//! The preflight is [`preflight`]; the profile, its state and `wipe` are in [`profile`].

mod connect;
mod preflight;
mod profile;
mod rehearse;
mod report;
mod stage;
#[cfg(test)]
mod tests;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};

use preflight::{Cleared, Ctx, RANGES_RS, RealGit, list_sha256, preflight};
use profile::{Where, disk_encryption, ensure, wipe_in};

use crate::{lock, root, target_dir};

const USAGE: &str = "usage: cargo xtask tg-run <command> [options]

The run tool of the Telegram backend (docs/telegram-run.md). `connect` is the ONLY command that
contacts Telegram: run it through kcap, and only after the user said go in that session.

commands:
  connect    one login QR code that nobody scans, asked through the gate, then quit; no account results
             --confirmed-by-user        the user said go in this session (required)
             --env test                 Telegram's test servers (required; production comes with R2)
             --profile <name>           the kept profile (default r0)
             --no-build                 reuse the stamped copies of the profile (they must match HEAD)
             --allow-unencrypted-disk   keep the profile on a disk without encryption (the user's call)
             --allow-unverified-ranges  start with an address list nobody verified (the user's call)
  rehearse   run the whole tool against the fake playing Telegram (throwaway place, no network)
             [--keep] [--no-build]   keep the place after a PASS / skip the build (an up-to-date debug app is there)
  wipe       delete the profile directory for good (never the API file)   --yes-delete-the-login
             [--profile <name>]
  ranges     print the list-sha256 of the address list of the working tree (for the VERIFIED line)

The API file is the user's own, written by himself; no secret is ever given on a command line.";

/// The default profile name.
const DEFAULT_PROFILE: &str = "r0";
/// The command line of the one real run, as the runbook states it and the preflight's refusal tells it:
/// through `kcap` (the CPU cap of his rule), and 1800 s, which covers the tool's own build. The runbook
/// test checks that the runbook holds it, the preflight test that the refusal names it.
const CONNECT_COMMAND: &str =
    "kcap -t 1800 cargo xtask tg-run connect --confirmed-by-user --env test";
/// The directory below the data home that holds the kept profiles and the API file (the app's
/// `RUN_DIR`; the X16 test pins the copy).
const KEEPER_DIR: &str = "kourier-tg-run";
/// The developer's API file in the keeper root (the app's `API_FILE`).
const API_FILE: &str = "telegram-api";
/// The tool's own directory inside a profile (the app's `DIAL_DIR` is the same name: the dial file
/// lives there).
const TOOL_DIR: &str = app_names::DIAL_DIR;
/// The rehearsal's places: `<target>/tgr/<secs>/x/kourier-tg-run/<name>`. Short on purpose: the app's
/// sockets below the place must fit a Unix socket path (107 bytes; critic 4).
const REHEARSAL_DIR: &str = "tgr";
/// The names the app and the tool agree on. Each is the tool's copy of the app's constant, which the
/// X16 test reads out of `crates/kourier-app/src` and compares; the walk uses them.
mod app_names {
    /// The debug switch of Telegram's servers (the app's `TELEGRAM_ENV`).
    pub const TELEGRAM_ENV: &str = "KOURIER_DEV_TELEGRAM_ENV";
    /// The variable that carries the nonce to the app (the app's `DIAL_ENV`).
    pub const DIAL_ENV: &str = "KOURIER_DEV_TELEGRAM_DIAL";
    /// The directory of the profile that holds the confirmation file (the app's `DIAL_DIR`).
    pub const DIAL_DIR: &str = "tg-run";
    /// The confirmation file (the app's `DIAL_FILE`).
    pub const DIAL_FILE: &str = "dial-go.json";
    /// The debug build's switch that puts the fake playing Telegram in the network's place (the
    /// app's `REHEARSAL_ENV`).
    pub const REHEARSAL_ENV: &str = "KOURIER_DEV_REHEARSAL";
}

// ---- The command line ---------------------------------------------------------------------------

/// What the command line says.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Args {
    pub command: String,
    pub place: Where,
    pub confirmed: bool,
    /// `--env <word>`; judged by the preflight (only `test` passes).
    pub env: Option<String>,
    pub allow_unencrypted: bool,
    pub allow_unverified_ranges: bool,
    pub no_build: bool,
    pub yes_delete: bool,
    /// `rehearse --keep`: leave the throwaway place after a PASS.
    pub keep: bool,
}

/// The refusal of anything that looks like a secret on a command line. It names no value and no
/// flag.
const NO_SECRETS_ON_A_COMMAND_LINE: &str = "tg-run takes no number, tail, code, password, API value, hash, nonce or token on a command line (a command line is shown by `ps` and written into logs and transcripts). The API file is the user's own, written by himself (docs/telegram-run.md). Nothing was started.";

/// The words that make a flag one that could carry a secret.
const SECRET_WORDS: &[&str] = &[
    "tail", "digit", "number", "phone", "msisdn", "api", "hash", "code", "password", "nonce",
    "token",
];

/// Whether a flag is one that could carry a secret (wa_run.rs:499 `is_a_digits_flag`, with
/// Telegram's words). The parser matches its KNOWN flags first, so `--allow-unverified-ranges` and
/// `--allow-unencrypted-disk` pass whatever letters they hold (critic 13).
fn is_a_secret_flag(arg: &str) -> bool {
    let flag = arg.split('=').next().unwrap_or(arg).to_ascii_lowercase();
    flag.starts_with("--") && SECRET_WORDS.iter().any(|w| flag.contains(w))
}

/// The command line (wa_run.rs:509 `parse_args`). An argument the parser does not know is never
/// echoed back, except the NAME of a flag that has only letters and dashes: what follows might be a
/// secret.
fn parse_args(args: &[String]) -> Result<Args> {
    let mut a = Args::default();
    let mut it = args.iter();
    let command = it.next().ok_or_else(|| anyhow!("{USAGE}"))?;
    if is_a_secret_flag(command) {
        bail!("{NO_SECRETS_ON_A_COMMAND_LINE}");
    }
    const COMMANDS: [&str; 5] = ["connect", "rehearse", "wipe", "ranges", "help"];
    a.command = match command.as_str() {
        c if COMMANDS.contains(&c) => c.to_owned(),
        "-h" | "--help" => "help".to_owned(),
        _ => bail!("unknown tg-run command (not shown: it might hold a secret)\n{USAGE}"),
    };
    while let Some(arg) = it.next() {
        let flag = arg.as_str();
        let mut value = |what: &str| {
            it.next()
                .cloned()
                .ok_or_else(|| anyhow!("{flag} needs {what}"))
        };
        match flag {
            "--profile" => a.place.profile = Some(value("a name")?),
            "--rehearsal-place" => {
                a.place.rehearsal_place = Some(PathBuf::from(value("a directory")?))
            }
            "--env" => a.env = Some(value("a word")?),
            "--confirmed-by-user" => a.confirmed = true,
            "--allow-unencrypted-disk" => a.allow_unencrypted = true,
            "--allow-unverified-ranges" => a.allow_unverified_ranges = true,
            "--no-build" => a.no_build = true,
            "--keep" => a.keep = true,
            "--yes-delete-the-login" => a.yes_delete = true,
            "-h" | "--help" => bail!("{USAGE}"),
            other => {
                if is_a_secret_flag(other) {
                    bail!("{NO_SECRETS_ON_A_COMMAND_LINE}");
                }
                let name = other.split('=').next().unwrap_or_default();
                if name.starts_with("--")
                    && name.bytes().all(|b| b.is_ascii_lowercase() || b == b'-')
                {
                    bail!("unknown argument {name}\n{USAGE}");
                }
                bail!("unknown argument (not shown: it might hold a secret)\n{USAGE}");
            }
        }
    }
    Ok(a)
}

// ---- The real context of the preflight -------------------------------------------------------------

/// The value of an environment variable by the presence rule of the preflight: a variable that is
/// set to bytes that are no UTF-8 is set all the same.
fn var(key: &str) -> Option<String> {
    std::env::var_os(key).map(|v| v.to_string_lossy().into_owned())
}

/// Runs `f` over the context the preflight reads from this machine.
fn with_real_ctx<T>(f: impl FnOnce(&Ctx<'_>) -> Result<T>) -> Result<T> {
    let root = root();
    let target = target_dir();
    let git = RealGit { root: &root };
    let cgroup = std::fs::read_to_string("/proc/self/cgroup").unwrap_or_default();
    let net_dev = std::fs::read_to_string("/proc/self/net/dev").unwrap_or_default();
    let busy = || {
        lock::lock_candidates().iter().any(|path| {
            let Ok(file) = std::fs::File::open(path) else {
                return false;
            };
            // A shared lock is refused while an exclusive one stands, and let go at once.
            matches!(
                file.try_lock_shared(),
                Err(std::fs::TryLockError::WouldBlock)
            )
        })
    };
    let library = || {
        crate::tdlib::checked_library(&root)
            .map(|(path, sha256)| stage::CheckedLibrary { path, sha256 })
    };
    // SAFETY: getuid has no preconditions and cannot fail.
    let uid = unsafe { libc::getuid() };
    let ctx = Ctx {
        vars: &var,
        root: &root,
        target: &target,
        git: &git,
        cgroup: &cgroup,
        disk: &disk_encryption,
        test_lock_busy: &busy,
        net_dev: &net_dev,
        uid,
        library: &library,
    };
    f(&ctx)
}

// ---- The commands ---------------------------------------------------------------------------------

/// `cargo xtask tg-run …`. Returns the exit status: 0, 2 for the usage, 1 when a check failed.
pub fn tg_run(args: &[String]) -> Result<i32> {
    if args.is_empty() {
        eprintln!("{USAGE}");
        return Ok(2);
    }
    let a = parse_args(args)?;
    match a.command.as_str() {
        "help" => {
            println!("{USAGE}");
            Ok(0)
        }
        "ranges" => ranges_command(),
        "wipe" => wipe_command(&a),
        "connect" => connect_command(&a),
        "rehearse" => rehearse::rehearse(&a),
        other => bail!("tg-run {other} is not a command of this tool"),
    }
}

/// `ranges`: the `list-sha256:` value of the address list in the WORKING TREE, for the VERIFIED line.
/// It opens nothing but the file and contacts nothing.
fn ranges_command() -> Result<i32> {
    let text = std::fs::read_to_string(root().join(RANGES_RS))?;
    println!("list-sha256:{}", list_sha256(&text)?);
    Ok(0)
}

/// `wipe` as the command line runs it.
fn wipe_command(a: &Args) -> Result<i32> {
    let vars = |k: &str| var(k);
    let dir = a.place.resolve(&vars, &target_dir())?;
    let (lines, status) = wipe_in(&dir, a.yes_delete, Duration::from_millis(400))?;
    for line in lines {
        println!("{line}");
    }
    Ok(status)
}

/// `connect`: the preflight, the profile, then the walk (`connect.rs`: the stage, the probe, the
/// confirmation, the app, the verdict). 0 for PASS, 1 for FAIL.
fn connect_command(a: &Args) -> Result<i32> {
    let cleared = with_real_ctx(|ctx| preflight(a, ctx))?;
    summary(a, &cleared);
    ensure(&cleared.dir, &cleared.state)?;
    connect::connect(a, cleared)
}

/// The words the preflight tells before anything starts: counts and facts, never a path.
fn summary(a: &Args, cleared: &Cleared) {
    for warning in &cleared.warnings {
        eprintln!("warning: {warning}");
    }
    println!("{}", summary_text(a, cleared));
}

/// The preflight's line. A fact the preflight did not look at says "not checked" (a rehearsal never
/// looks at a disk).
fn summary_text(a: &Args, cleared: &Cleared) -> String {
    format!(
        "preflight: profile {} (env test); disk {}; build {}; address list {}; library {}; {}",
        cleared.dir.label(),
        match cleared.encrypted {
            Some(true) => "encrypted",
            Some(false) => "not encrypted",
            None => "not checked",
        },
        cleared
            .head
            .as_deref()
            .map_or("not checked", |h| &h[..h.len().min(10)]),
        match cleared.ranges_verified {
            Some(true) => "verified",
            Some(false) => "NOT verified",
            None => "not checked",
        },
        if cleared.library.is_some() {
            "checked"
        } else {
            "not checked"
        },
        if a.no_build {
            "stamped copies reused"
        } else {
            "build first"
        },
    )
}
