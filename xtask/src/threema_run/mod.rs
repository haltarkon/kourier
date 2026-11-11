//! `cargo xtask threema-run <command>`: the run tool of the Threema backend (TR-lite, the tool of the
//! first real Threema run; the runbook is `docs/threema-run.md`). It is `wa_run.rs`'s detached
//! supervisor, cut, and `tg_run/`'s profile rules: the app runs on a kept profile, the tool holds it,
//! answers one step at a time and halts it on a stop rule.
//!
//! ```text
//! cargo xtask threema-run start --confirmed-by-user [--for-link] [--profile <name>]   # the only command that contacts Threema
//! target/debug/xtask threema-run id [--profile <name>]                              # HIS terminal: the test ID, typed, never given here
//! cargo xtask threema-run step <name> [--seconds <s>] [--profile <name>]            # one step of the run (`steps` lists them)
//! cargo xtask threema-run wipe [--profile <name>] --yes-delete-the-login
//! ```
//!
//! This file is the command line, the usage and the dispatch; the kept profile, its state, the places
//! it may not live in and `wipe` are in [`profile`], the start's checks in [`preflight`], and what
//! `start` does after them (the stamped copy, the app's environment, the probe, the control socket,
//! the switch and its guard, the detached supervisor) in [`launch`]. The supervisor itself (the app's
//! boot, its two readers, the redaction, the control socket's operations, `state`, `stop` and the
//! end of a run) is [`supervise`]; what the user types in his own terminal (`id`, `answer`) and the
//! list of test IDs' whole life are [`prompt`]. Every line the tool reads from the app's log is in
//! [`lines`] (the one table, its parser) and the stop rules S1-S12 are [`judge`], one function the
//! supervisor calls for every line and event in order. The steps (`step <name>`, `steps`: `link`, `sync`,
//! `receive`, `group-receive`, `mirror`, `reconnect`, `passive`, then `kinds`, `phone-sleep`, `flight-mode`,
//! `removed-on-phone` and `unlink`, and the runner that writes each call to `steps.jsonl`) are [`steps`];
//! `report` (the profile's files into `threema-run/report.md`) is [`report`]. `rehearse` (the whole tool
//! walked against the fake that plays Threema, in `unshare -rn`, no network) is [`rehearse`]. `backup` (the
//! kept profile copied while no app runs, never the list of test IDs) is [`backup`]. Nothing in this file
//! itself starts a process or contacts a host.
//!
//! # The rules this tool keeps
//!
//! * Nothing contacts Threema except `start --confirmed-by-user`, and that only after the user's go in
//!   that session, for EVERY start. Tests, rehearsals and agents never contact it or any other host.
//! * The test ID is typed by the user in `id`, in his own terminal, hashed there; only the digest goes
//!   on. It is NEVER on a command line, in an environment variable, a log, a report or a file of the
//!   tool's: the parser refuses an argument that has its shape, the shape of a digest of it, or a flag
//!   that names it, with one fixed sentence that echoes nothing. A digest of an eight-character ID is
//!   as good as the ID (36^8 tries), so it is treated as the ID.
//! * A kept profile lives only in `${XDG_DATA_HOME:-~/.local/share}/kourier-threema-run/<name>` (0700),
//!   outside the repository, its worktrees, `~/projects` and its build directories, and apart from
//!   every other tool's data and from the app's own data directory.
//! * What the tool prints or writes is meant for someone who must not see names, IDs or texts:
//!   counts, words and times only.

mod backup;
mod judge;
mod launch;
mod lines;
mod preflight;
mod profile;
mod prompt;
mod rehearse;
mod report;
mod steps;
mod supervise;
#[cfg(test)]
mod tests;
mod tphone;
mod tphone_fake;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};

use profile::{Where, wipe_in};

use crate::target_dir;

const USAGE: &str = "usage: cargo xtask threema-run <command> [options]

The run tool of the Threema backend (docs/threema-run.md). `start` is the ONLY command that contacts
Threema (through the app): run it through kcap, and only after the user said go in that session.
The test ID is typed by the user himself in `id`, in his own terminal, and is never given here.

commands:
  start      start the app on the profile and keep it running
             --confirmed-by-user        the user said go in this session (required, for every start)
             --for-link                 wait for the ID (`id`) so that a first link can follow
             --after-halt               start again after the run halted (the user's go)
             --allow-schema-change      start a build whose database schema differs from the profile's
             --allow-unencrypted-disk   keep a real login on a disk without encryption (the user's call)
             --no-build                 reuse the stamped copy of the app
             --test-phone <serial>      the TEST phone's adb serial: the steps check and act on it
                                        (kept in the profile; the flag wins over the kept value)
             --no-test-phone            the profile forgets its test phone
  id         the user types the TEST ID here, in his own terminal, with nothing shown; it takes no value;
             run the built binary: target/debug/xtask threema-run id [--profile <name>]
  id --from-test-phone   (a run with a test phone) read the TEST ID from the TEST phone's My ID
  answer --relayed y|n --question <name>   (a run with a test phone) the lead relays the user's y or n after his
             word in chat; <name> is the one in the `ASK-USER [<name>]` line, and it is taken only when that
             question is the open one
  answer     the user types y or n to the question a step asks here, in his own terminal, one key; run
             the built binary: target/debug/xtask threema-run answer [--profile <name>]
  step <name> [--seconds <s>] [--user-said-yes] [--lead-sends]
             one named step (see `steps`); --user-said-yes: the user said yes in chat to the
             step's risky act (only the lead passes it; a run with a test phone needs it for `unlink`
             and `removed-on-phone`); --lead-sends: `send`, `group-send` and `send-file` make their
             one send themselves (only the lead passes it, after the user's word in chat)
  steps      list the steps in their order
  state      the profile and the running app, as counts and states
  report     write threema-run/report.md into the profile directory
  stop       stop the app; the login stays; Threema is switched off in the profile   [--force]
  backup     copy the kept profile while no app runs: to backups/<name>-<UTC time> in the keeper root,
             or into <dir>; never the list of test IDs   [--to <dir>] [--allow-unencrypted-disk]
  wipe       delete the profile directory for good   --yes-delete-the-login
  rehearse   run the whole tool against the fake playing Threema (throwaway place, no network)
             [--keep] [--no-build]

profile (every command except steps and rehearse): --profile <name> (default \"rt1\")";

/// The default profile name.
const DEFAULT_PROFILE: &str = "rt1";
/// The directory below the data home that holds the kept profiles and the list of test IDs (the
/// app's `RUN_DIR`; a test pins the copy).
const KEEPER_DIR: &str = "kourier-threema-run";
/// The list of test IDs in the keeper root (the app's `ALLOW_FILE`; a test pins the copy). It lives
/// seconds, only in a `--for-link` start.
const ALLOW_FILE: &str = "threema-allowed-ids";
/// The temporary name the list is written under before the rename (`write_private` adds the extension
/// `tmp`; a test pins that): it holds the digest too, so every cleanup removes it as well (LOW-1).
const ALLOW_TMP: &str = "threema-allowed-ids.tmp";
/// The tool's own directory inside a profile.
const TOOL_DIR: &str = "threema-run";
/// The keeper lock in the keeper root: held from the moment the list is written until it is gone.
const KEEPER_LOCK: &str = ".threema-run.lock";
/// The open question of a failed link, in the keeper root (critic M4): the kind and the time, no
/// other value. A `wipe` and a link start refuse while it stands.
const OPEN_QUESTION_FILE: &str = ".threema-run-open-question";
/// The default place of `backup` below the keeper root; never a profile (the name is reserved).
const BACKUPS_DIR: &str = "backups";
/// The rehearsal's places: `<target>/thr/<secs>/x/kourier-threema-run/<name>`. Short on purpose; the
/// supervisor's control socket does not live below the place at all.
const REHEARSAL_DIR: &str = "thr";
/// How long `wipe` waits for the profile's lock (a lock that a program another thread just started
/// carries until its `exec` is not a lock held).
const WIPE_SETTLE: Duration = Duration::from_millis(400);

// ---- The command line ---------------------------------------------------------------------------

/// What the command line says.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Args {
    pub command: String,
    /// `step <name>`.
    pub step: Option<String>,
    pub place: Where,
    pub confirmed: bool,
    pub for_link: bool,
    pub after_halt: bool,
    pub allow_schema_change: bool,
    pub allow_unencrypted: bool,
    pub no_build: bool,
    pub force: bool,
    /// `rehearse --keep`: leave the throwaway place after a PASS.
    pub keep: bool,
    /// `backup --to <dir>`.
    pub to: Option<PathBuf>,
    pub yes_delete: bool,
    /// `step passive --seconds <s>`.
    pub seconds: Option<u64>,
    /// `start --test-phone <serial>`: the TEST phone's adb serial (`host:port` or a USB serial).
    pub test_phone: Option<String>,
    /// `start --no-test-phone`: the profile forgets its test phone (a start without it is as before).
    pub no_test_phone: bool,
    /// `step <name> --user-said-yes`: the lead passes it only after the user's word in chat.
    pub user_said_yes: bool,
    /// `step send|group-send|send-file --lead-sends`: the step makes its one send itself; the lead
    /// passes it only after the user's word in chat.
    pub lead_sends: bool,
    /// `id --from-test-phone`: read the TEST ID from the TEST phone's My ID.
    pub from_test_phone: bool,
    /// `answer --relayed y|n`: the lead relays the user's word (needs a run with a test phone).
    pub relayed: Option<bool>,
    /// `answer --relayed y|n --question <name>`: the question the relay answers (taken only when it is the
    /// open one).
    pub question: Option<String>,
}

/// The refusal of anything that looks like the test ID, a digest of it or a flag that names it, on a
/// command line. It names no value and no flag.
const NO_SECRETS_ON_A_COMMAND_LINE: &str = "threema-run takes no Threema ID, no digest of one and nothing else of the test account on a command line (a command line is shown by `ps`, kept in the shell's history and written into logs and transcripts). The ID is typed only in `id`, by the user, in his own terminal. Nothing was started.";

/// The words that make a flag one that could carry the ID or a value derived from it: the ID itself,
/// the network, a digest of it, the other secret words of the siblings' lists (the sentence
/// `NO_SECRETS_ON_A_COMMAND_LINE` is the same for all of them).
const SECRET_WORDS: &[&str] = &[
    "id", "threema", "digest", "hash", "sha", "key", "code", "token", "number", "nonce",
    "password", "secret", "phone", "tail", "digit",
];

/// Whether a flag is one that could carry the ID or a value derived from it (wa_run.rs:605
/// `is_a_digits_flag`, tg_run `is_a_secret_flag`, with Threema's words). The parser matches its
/// KNOWN flags first, so a documented flag passes whatever letters it holds.
fn is_a_secret_flag(arg: &str) -> bool {
    let flag = arg.split('=').next().unwrap_or(arg).to_ascii_lowercase();
    flag.starts_with("--") && SECRET_WORDS.iter().any(|w| flag.contains(w))
}

/// Whether `text` has the shape of a Threema ID: exactly eight characters, letters and digits in
/// either case (the first may be a `*`, a Gateway ID). A word of eight letters has it too: the
/// refusal costs nothing, and the typed ID could be one.
fn looks_like_an_id(text: &str) -> bool {
    text.len() == 8
        && text
            .bytes()
            .enumerate()
            .all(|(i, b)| b.is_ascii_alphanumeric() || (i == 0 && b == b'*'))
}

/// Whether `text` holds a run of 64 or more hexadecimal digits: a SHA-256 digest, bare or after a
/// `sha256:` prefix or any other glue (the allow list's lines have the form `sha256:` and 64 digits).
fn holds_a_digest(text: &str) -> bool {
    let mut run = 0;
    for b in text.bytes() {
        if b.is_ascii_hexdigit() {
            run += 1;
            if run >= 64 {
                return true;
            }
        } else {
            run = 0;
        }
    }
    false
}

/// Whether a word or a value of the command line is the ID or a digest of it.
fn looks_like_a_secret_value(text: &str) -> bool {
    looks_like_an_id(text) || holds_a_digest(text)
}

/// The command line (wa_run.rs:615 `parse_args`, tg_run `parse_args`). An argument the parser does
/// not know is never echoed back, except the NAME of a flag that has only letters and dashes: what
/// follows might be the ID.
fn parse_args(args: &[String]) -> Result<Args> {
    let mut a = Args::default();
    let mut it = args.iter();
    let command = it.next().ok_or_else(|| anyhow!("{USAGE}"))?;
    // A digest, or a flag that names the ID, can never be a command; the shape of an ID can only be
    // one that is not a command (`rehearse` has eight letters).
    if is_a_secret_flag(command) || holds_a_digest(command) {
        bail!("{NO_SECRETS_ON_A_COMMAND_LINE}");
    }
    const COMMANDS: [&str; 13] = [
        "start",
        "id",
        "answer",
        "step",
        "steps",
        "state",
        "report",
        "stop",
        "backup",
        "wipe",
        "rehearse",
        "help",
        // Hidden: `start` starts it, nothing else should.
        "__supervise",
    ];
    a.command = match command.as_str() {
        c if COMMANDS.contains(&c) => c.to_owned(),
        "-h" | "--help" => "help".to_owned(),
        other if looks_like_an_id(other) => bail!("{NO_SECRETS_ON_A_COMMAND_LINE}"),
        _ => bail!("unknown threema-run command (not shown: it might hold the ID)\n{USAGE}"),
    };
    if a.command == "step" {
        let name = it
            .next()
            .filter(|s| !s.starts_with("--"))
            .ok_or_else(|| anyhow!("step needs a name (see `cargo xtask threema-run steps`)"))?;
        // A step name is lower-case letters and dashes, and one of eight letters has the shape of an
        // ID: only a name outside that grammar is judged by its shape (the step table, not this
        // parser, says which names exist, and an unknown one is never echoed).
        if holds_a_digest(name) {
            bail!("{NO_SECRETS_ON_A_COMMAND_LINE}");
        }
        if !name.bytes().all(|b| b.is_ascii_lowercase() || b == b'-') {
            if looks_like_an_id(name) {
                bail!("{NO_SECRETS_ON_A_COMMAND_LINE}");
            }
            bail!(
                "a step name has lower-case letters and dashes only (see `cargo xtask threema-run steps`)"
            );
        }
        a.step = Some(name.clone());
    }
    while let Some(arg) = it.next() {
        let flag = arg.as_str();
        let mut value = |what: &str| -> Result<String> {
            let v = it
                .next()
                .cloned()
                .ok_or_else(|| anyhow!("{flag} needs {what}"))?;
            if looks_like_a_secret_value(&v) {
                bail!("{NO_SECRETS_ON_A_COMMAND_LINE}");
            }
            Ok(v)
        };
        match flag {
            "--profile" => a.place.profile = Some(value("a name")?),
            "--rehearsal-place" => {
                a.place.rehearsal_place = Some(PathBuf::from(value("a directory")?))
            }
            "--confirmed-by-user" => a.confirmed = true,
            "--for-link" => a.for_link = true,
            "--after-halt" => a.after_halt = true,
            "--allow-schema-change" => a.allow_schema_change = true,
            "--allow-unencrypted-disk" => a.allow_unencrypted = true,
            "--no-build" => a.no_build = true,
            "--force" => a.force = true,
            "--keep" => a.keep = true,
            "--to" => a.to = Some(PathBuf::from(value("a directory")?)),
            "--yes-delete-the-login" => a.yes_delete = true,
            "--test-phone" => a.test_phone = Some(value("an adb serial")?),
            "--no-test-phone" => a.no_test_phone = true,
            "--user-said-yes" => a.user_said_yes = true,
            "--lead-sends" => a.lead_sends = true,
            "--from-test-phone" => a.from_test_phone = true,
            "--relayed" => {
                a.relayed = Some(match value("y or n")?.as_str() {
                    "y" => true,
                    "n" => false,
                    _ => bail!("--relayed takes y or n"),
                });
            }
            "--question" => a.question = Some(value("a question name")?),
            "--seconds" => {
                a.seconds = Some(
                    value("seconds")?
                        .parse()
                        .map_err(|_| anyhow!("--seconds needs a number of seconds"))?,
                );
            }
            "-h" | "--help" => bail!("{USAGE}"),
            other => {
                // The name of the flag, a `=` value, and the whole argument: any of them may be it.
                let (name, value) = other.split_once('=').unwrap_or((other, ""));
                if is_a_secret_flag(other)
                    || looks_like_a_secret_value(other)
                    || looks_like_a_secret_value(value)
                {
                    bail!("{NO_SECRETS_ON_A_COMMAND_LINE}");
                }
                if name.starts_with("--")
                    && name.bytes().all(|b| b.is_ascii_lowercase() || b == b'-')
                {
                    bail!("unknown argument {name}\n{USAGE}");
                }
                bail!("unknown argument (not shown: it might hold the ID)\n{USAGE}");
            }
        }
    }
    Ok(a)
}

// ---- The commands ---------------------------------------------------------------------------------

/// The value of an environment variable by the presence rule of the checks: a variable that is set
/// to bytes that are no UTF-8 is set all the same.
fn var(key: &str) -> Option<String> {
    std::env::var_os(key).map(|v| v.to_string_lossy().into_owned())
}

/// `cargo xtask threema-run …`. Returns the exit status: 0, 2 for the usage, 1 when a check failed.
pub fn threema_run(args: &[String]) -> Result<i32> {
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
        "wipe" => wipe_command(&a),
        "start" => launch::start(&a),
        "__supervise" => supervise::supervise(&a),
        "state" => supervise::state_command(&a),
        "stop" => supervise::stop_command(&a),
        "id" => prompt::id_command(&a),
        "answer" => prompt::answer_command(&a),
        "step" => steps::step_command(&a),
        "steps" => steps::steps_command(&a),
        "report" => report::report_command(&a),
        "rehearse" => rehearse::rehearse(&a),
        "backup" => backup::backup_command(&a),
        other => bail!("threema-run {other} is not a command of this tool"),
    }
}

/// `wipe` as the command line runs it. Without its flag it deletes nothing and says so.
fn wipe_command(a: &Args) -> Result<i32> {
    let dir = a.place.resolve(&var, &target_dir())?;
    let (lines, status) = wipe_in(&dir, a.yes_delete, WIPE_SETTLE)?;
    for line in lines {
        println!("{line}");
    }
    Ok(status)
}
