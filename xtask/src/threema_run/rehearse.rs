//! `cargo xtask threema-run rehearse`: the whole tool, walked end to end against the fake that plays
//! Threema, with no network (T9; `briefs/tr.md` §2.13, the critic's C8 and C11; the form of
//! `tg_run/rehearse.rs`, with the pseudo-terminal helpers of `wa_run.rs`'s rehearsal).
//!
//! The walk takes the test lock, builds the app ONCE outside any namespace (cargo inside `unshare -rn`
//! changes its fingerprints and rebuilds both ways), and then runs the tool's own commands as separate
//! processes, EACH in its own `unshare -rn` (only `lo`; the tool's preflight refuses a rehearsal that
//! sees another interface), offscreen and silent, with the places below
//! `<target>/thr/<secs>/<x|w|k>/kourier-threema-run/` and the control socket in a short place of
//! `/dev/shm` ([`super::launch::rehearsal_socket_dir`]: nothing depends on the checkout's length).
//! Nothing in it is Threema's: `KOURIER_DEV_REHEARSAL=threema` puts the fake in the network's place, no
//! dialer or library is made, and no host is reached. The user is the walk: his terminal is a
//! pseudo-terminal of the walk's own, the TEST ID he types is the fake's own (`REHOWN01`), his y and n
//! are typed into `threema-run answer`, his click in the window is the supervisor's rehearsal-only cue
//! `add_account` ([`super::supervise::rehearsal_cue_ok`]) and the PERSONAL phone's text is its other
//! cue, `rehearsal_arrive`.
//!
//! Its checks, each one `passed += 1`:
//!
//!  1. `start` without the user's go on a kept keeper root is refused and makes nothing;
//!  2. `id` with no run is refused and asks nothing;
//!  3. `start --for-link`: the switch ON, the probe registers Threema, the supervisor waits for the ID
//!     and cannot be dumped, no list stands;
//!  4. `id` on a terminal (the fake's own ID typed twice, nothing shown): taken; THE LIST'S WHOLE LIFE:
//!     seen standing (0600 in a 0700 root, the digest's line), gone at the app's first answer, the
//!     keeper lock free; a second `id` and an `answer` with no question are refused;
//!  5. `step link` (the question "is it the TEST phone", the click, the QR, the emoji, the end) passes
//!     with every answer given on a terminal, then `sync`, `receive` and `group-receive` (an arrival
//!     on cue), `passive`; `unlink` with a "no" to `end-go` changes nothing;
//!  6. `state` and `stop`: the switch off, nothing runs, no list; the probe no longer registers Threema;
//!  7. `backup` of the stopped profile (a list planted beside it): one 0700 copy with the state and the
//!     data, no list, no stamped program; the planted list is removed by the next `start` with its
//!     warning, that start (no link) refuses `id`, and `reconnect` finds the chats as they were left;
//!  8. `report` writes its file; `wipe` without and with its flag;
//!  9. THE LEAK SEARCH: both synthetic IDs, their digests, the group's key, the three names and the
//!     arrival's words, in every file of the places but the stamped program, and in everything the
//!     commands and the terminals printed: none; no digest-shaped run in the tool's files.
//!
//! NOT SHOWN, and said so in the end line (C8): `flight-mode`, `phone-sleep` and `kinds` (the fake
//! prints none of the contract lines: no leader, no D1, no counts), `mirror` (the fake has no act that
//! writes an own message of the phone), the real removals (`removed-on-phone`, `unlink` itself: the
//! dev control has no remove command), the wrong-ID refusal (the fake has no ID refusal: the backend's
//! tests prove B1's gate), the real join and warning text, the window, and a search of the
//! app's RAW stderr (the supervisor redacts before it writes: the markers it left are counted).

use std::fs::File;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::os::fd::{AsRawFd as _, FromRawFd as _};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

use super::launch::{
    RealClock, Wait, probe_command, real_build, run_bounded, socket_for_client, switch_is_on,
};
use super::preflight::{NETWORK_NAME, TEST_ENV, check_slice};
use super::profile::{ProfileDir, State, ensure, private_dir, write_private};
use super::prompt::{PROMPT_AGAIN, PROMPT_FIRST, digest_of_id, list_text};
use super::supervise::{LIST_TAKEN, call_at};
use super::{ALLOW_FILE, Args, KEEPER_DIR, REHEARSAL_DIR, TOOL_DIR, holds_a_digest};
use crate::{lock, target_dir};

/// How long one command of the walk may run (the longest are `step link`, which waits for the fake's
/// twenty seconds of emoji, and a `stop`, which waits for the app to quit).
const CHILD_BOUND: Duration = Duration::from_secs(300);
/// How long the walk waits for something the supervisor does (a list taken, an account online).
const WAIT_BOUND: Duration = Duration::from_secs(90);
/// The names of the places below `<target>/thr/<secs>/`: the data home of the walk's profile, of the
/// wipe's profile and of the scratch keeper root of check 1.
const WALK_DATA: &str = "x";
const WIPE_DATA: &str = "w";
/// The place of the walk with a TEST phone (the scripted fake; the tool never runs adb in a rehearsal).
const PHONE_DATA: &str = "t";
/// The serial the walk names (a rehearsal's phone is the scripted fake whatever it says).
const PHONE_SERIAL: &str = "rehearsal-phone";
const SCRATCH_DATA: &str = "k";
const WALK_PROFILE: &str = "p";
const WIPE_PROFILE: &str = "w";
/// What the go check says; check 1 looks for it.
const GO_REFUSAL: &str = "needs the user's go";
/// What `wipe` ends in without its flag, and with it.
const WIPE_UNDONE: &str = "Nothing was deleted";
const WIPE_DONE: &str = "deleted.";
/// The fake's ID typed as the test ID, and the words `id` and `answer` end in.
const TAKEN: &str = "taken (8 characters)";
/// The tail of the prompts the terminal waits for (the full texts are `prompt.rs`'s).
const ANSWER_PROMPT_TAIL: &str = "[y/n]: ";

// ---- The fake's constants ---------------------------------------------------------------------------

/// The tool's copy of every string constant of `crates/kourier-fake/src/threema.rs`, in the file's
/// order (a test reads that file and pins these copies, names and values; xtask links no fake).
pub(super) const FAKE_CONSTANTS: [(&str, &str); 7] = [
    ("OWN_NAME", "Rehearsal Thowner"),
    ("PEER_NAME", "Rehearsal Thpeer"),
    ("GROUP_NAME", "Rehearsal Thgroup"),
    ("OWN_ID", "REHOWN01"),
    ("PEER_ID", "REHPEER2"),
    ("GROUP_ID_HEX", "7c1e5a93b04d2f68"),
    ("ARRIVAL_WORDS", "Rehearsal arrival"),
];

/// The value of one of [`FAKE_CONSTANTS`] by its name.
pub(super) fn constant(name: &str) -> &'static str {
    FAKE_CONSTANTS
        .iter()
        .find(|(n, _)| *n == name)
        .map_or("", |(_, v)| *v)
}

// ---- What no file or line of the walk may hold ---------------------------------------------------------

/// One thing no file or line of the rehearsal may hold: a label (what to call a hit, never the value)
/// and the text.
#[derive(Clone, Debug)]
pub(super) struct Needle {
    pub label: String,
    value: String,
}

/// Every needle: the fake's constants, and the digests of the two IDs (the list's line holds one of
/// them: a digest of an eight-character ID is as good as the ID).
pub(super) fn needles() -> Result<Vec<Needle>> {
    let mut out: Vec<Needle> = FAKE_CONSTANTS
        .iter()
        .map(|(name, value)| Needle {
            label: format!("the fake's {name}"),
            value: (*value).to_owned(),
        })
        .collect();
    for name in ["OWN_ID", "PEER_ID"] {
        out.push(Needle {
            label: format!("the digest of the fake's {name}"),
            value: digest_of_id(constant(name).as_bytes())?,
        });
    }
    Ok(out)
}

/// The labels of the needles that stand in `bytes`.
pub(super) fn hits(bytes: &[u8], needles: &[Needle]) -> Vec<String> {
    let text = String::from_utf8_lossy(bytes);
    needles
        .iter()
        .filter(|n| !n.value.is_empty() && text.contains(&n.value))
        .map(|n| n.label.clone())
        .collect()
}

/// Every file below `dir`, found with [`hits`] and, in the tool's own files (`threema-run/`, the stamped
/// program aside), with the shape of a digest too: `(path relative to dir, what)`. The stamped program
/// (`threema-run/bin`) is the program and holds the fake's names as code, and its stamp holds the
/// program's own hash. A file that cannot be read is a finding of its own.
pub(super) fn leaks_in(dir: &Path, needles: &[Needle]) -> Vec<(String, String)> {
    fn walk(at: &Path, root: &Path, needles: &[Needle], out: &mut Vec<(String, String)>) {
        let Ok(entries) = std::fs::read_dir(at) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let relative = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .into_owned();
            if kind.is_dir() {
                if !path.ends_with(Path::new(TOOL_DIR).join("bin")) {
                    walk(&path, root, needles, out);
                }
            } else if kind.is_file() {
                match std::fs::read(&path) {
                    Ok(bytes) => {
                        for label in hits(&bytes, needles) {
                            out.push((relative.clone(), label));
                        }
                        // The tool's own files and the keeper root's: no digest-shaped run at all.
                        let parts: Vec<_> = Path::new(&relative).components().collect();
                        let tool =
                            parts.len() <= 2 || parts.iter().any(|c| c.as_os_str() == TOOL_DIR);
                        if tool && holds_a_digest(&String::from_utf8_lossy(&bytes)) {
                            out.push((relative, "a run of 64 hex digits (a digest?)".to_owned()));
                        }
                    }
                    Err(_) => out.push((relative, "a file the search could not read".to_owned())),
                }
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, needles, &mut out);
    out
}

/// How many redaction markers (`[id]`, `[digest]`) `text` holds: what the supervisor's redaction
/// caught of the app's stderr before it wrote it (the raw stderr is never kept).
pub(super) fn redaction_markers(text: &str) -> (usize, usize) {
    (
        text.matches("[id]").count(),
        text.matches("[digest]").count(),
    )
}

// ---- The places ----------------------------------------------------------------------------------------

/// A profile directory of the rehearsal: `<target>/thr/<secs>/<data>/kourier-threema-run/<name>`. The
/// data home (`<data>`) is the app's `XDG_DATA_HOME`, so its own run-profile rule holds in the
/// rehearsal too.
pub(super) fn place_under(target: &Path, secs: u64, data: &str, name: &str) -> PathBuf {
    target
        .join(REHEARSAL_DIR)
        .join(secs.to_string())
        .join(data)
        .join(KEEPER_DIR)
        .join(name)
}

fn unix_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

fn is_empty_dir(dir: &Path) -> bool {
    std::fs::read_dir(dir).is_ok_and(|mut d| d.next().is_none())
}

fn dir_size(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => dir_size(&e.path()),
            Ok(t) if t.is_file() => e.metadata().map_or(0, |m| m.len()),
            _ => 0,
        })
        .sum()
}

// ---- Reading what the commands printed -----------------------------------------------------------------

/// The first line of `printed` that starts (after its blanks) with `prefix`.
fn line_with<'a>(printed: &'a str, prefix: &str) -> Option<&'a str> {
    printed
        .lines()
        .map(str::trim_start)
        .find(|l| l.starts_with(prefix))
}

/// How a step's call ended, by its own words: `PASS <name> (...)` or `FAIL <name> (...): why`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Verdict {
    pub pass: bool,
    pub reason: String,
}

pub(super) fn verdict_of(printed: &str, step: &str) -> Option<Verdict> {
    printed.lines().rev().find_map(|l| {
        let l = l.trim_start();
        let (pass, rest) = l
            .strip_prefix("PASS ")
            .map(|r| (true, r))
            .or_else(|| l.strip_prefix("FAIL ").map(|r| (false, r)))?;
        let rest = rest.strip_prefix(step)?;
        rest.starts_with(' ').then(|| Verdict {
            pass,
            reason: rest
                .split_once("): ")
                .map_or_else(String::new, |(_, r)| r.to_owned()),
        })
    })
}

/// The kinds one list of the probe's `networks registered=<kinds> off=<kinds> on=<kinds>` line names;
/// `None` when the output holds no such line.
pub(super) fn kinds_of(stdout: &str, key: &str) -> Option<Vec<String>> {
    let line = stdout.lines().find(|l| l.starts_with("networks "))?;
    let list = line
        .split_whitespace()
        .find_map(|w| w.strip_prefix(key))
        .unwrap_or_default();
    Some(
        list.split(',')
            .filter(|k| !k.is_empty())
            .map(str::to_owned)
            .collect(),
    )
}

/// `cmd` run as `unshare -rn <cmd>`: the same program, arguments and environment changes (the probe the
/// walk runs by itself must see no network either).
pub(super) fn in_unshare(cmd: &Command) -> Command {
    let mut out = Command::new("unshare");
    out.arg("-rn").arg(cmd.get_program()).args(cmd.get_args());
    for (key, value) in cmd.get_envs() {
        match value {
            Some(value) => out.env(key, value),
            None => out.env_remove(key),
        };
    }
    out
}

// ---- A terminal of the walk's own (wa_run.rs:10684 `spawn_in_a_terminal`) ---------------------------------

/// Starts `cmd` with a terminal of its own (a pseudo-terminal that is its controlling terminal, the way
/// the user's own terminal is `id`'s and `answer`'s), its standard output and error piped. Returns the
/// child and the terminal's master side.
fn spawn_in_a_terminal(mut cmd: Command) -> Result<(Child, File)> {
    let (mut master, mut slave) = (0, 0);
    // SAFETY: `openpty` fills two descriptors; the null pointers ask for no name, termios or size.
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    if rc != 0 {
        bail!("openpty failed: no terminal of its own for the rehearsal's user");
    }
    // SAFETY: both descriptors are new and owned here; the master must not leak into children.
    let (master, slave) = unsafe {
        libc::fcntl(master, libc::F_SETFD, libc::FD_CLOEXEC);
        (File::from_raw_fd(master), File::from_raw_fd(slave))
    };
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let slave_fd = slave.as_raw_fd();
    // SAFETY: only `setsid` and `ioctl` run between fork and exec, both async-signal-safe.
    unsafe {
        cmd.pre_exec(move || {
            if libc::setsid() == -1 || libc::ioctl(slave_fd, libc::TIOCSCTTY, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = cmd.spawn().context("start a command on a terminal")?;
    drop(slave);
    Ok((child, master))
}

/// Reads what is on the terminal for at most `wait_ms`.
fn read_some(master: &File, seen: &mut Vec<u8>, wait_ms: i32) {
    let mut poll = libc::pollfd {
        fd: master.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one valid `pollfd`.
    if unsafe { libc::poll(&mut poll, 1, wait_ms) } > 0 && poll.revents & libc::POLLIN != 0 {
        let mut buf = [0u8; 512];
        // SAFETY: `buf` is valid for its length.
        let n = unsafe { libc::read(master.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
        if let Ok(n) = usize::try_from(n) {
            seen.extend_from_slice(&buf[..n]);
        }
    }
}

/// What a command on a terminal did: its status, what the terminal showed and what it printed.
#[derive(Debug)]
pub(super) struct OnATerminal {
    pub code: Option<i32>,
    pub screen: String,
    pub stdout: String,
    pub stderr: String,
}

/// Runs `cmd` on a terminal of its own and types for the user: for each `(marker, text)` it waits until
/// the terminal shows `marker` (after what the earlier markers consumed) and then writes `text`; then it
/// waits for the command to end. A marker that never shows, or a command that outlives `bound`, kills
/// the child and fails (the screen so far is in the error's words, which never hold a typed value).
pub(super) fn run_on_a_terminal(
    cmd: Command,
    typing: &[(&str, &str)],
    bound: Duration,
) -> Result<OnATerminal> {
    let (mut child, master) = spawn_in_a_terminal(cmd)?;
    let read_all = |mut pipe: Box<dyn std::io::Read + Send>| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = pipe.read_to_end(&mut bytes);
            String::from_utf8_lossy(&bytes).into_owned()
        })
    };
    let out = read_all(Box::new(child.stdout.take().context("stdout")?));
    let err = read_all(Box::new(child.stderr.take().context("stderr")?));
    let deadline = Instant::now() + bound;
    let mut screen = Vec::new();
    let mut consumed = 0;
    let mut failed: Option<String> = None;
    'typing: for (marker, text) in typing {
        loop {
            if String::from_utf8_lossy(&screen[consumed..]).contains(marker) {
                consumed = screen.len();
                let mut writer = &master;
                if writer.write_all(text.as_bytes()).is_err() {
                    failed = Some("the terminal did not take the keys".to_owned());
                    break 'typing;
                }
                break;
            }
            if Instant::now() >= deadline || child.try_wait()?.is_some() {
                failed = Some(format!(
                    "the terminal never showed the prompt `{marker}`; it showed `{}`",
                    String::from_utf8_lossy(&screen[consumed..]).replace(['\r', '\n'], " ")
                ));
                break 'typing;
            }
            read_some(&master, &mut screen, 50);
        }
    }
    let mut status = None;
    if failed.is_none() {
        while Instant::now() < deadline {
            read_some(&master, &mut screen, 50);
            if let Some(s) = child.try_wait()? {
                status = Some(s);
                break;
            }
        }
        if status.is_none() {
            failed = Some("the command did not end in time".to_owned());
        }
    }
    if let Some(why) = failed {
        let _ = child.kill();
        let _ = child.wait();
        bail!("{why}");
    }
    // What the terminal still holds after the end.
    for _ in 0..5 {
        read_some(&master, &mut screen, 40);
    }
    Ok(OnATerminal {
        code: status.and_then(|s| s.code()),
        screen: String::from_utf8_lossy(&screen).into_owned(),
        stdout: out.join().unwrap_or_default(),
        stderr: err.join().unwrap_or_default(),
    })
}

// ---- What the walk does for the user ------------------------------------------------------------------------

/// What the walk does, besides answering, when a step has said something or he has answered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Cue {
    /// The click of "Add Account", Threema: the supervisor's `add_account` cue.
    Click,
    /// The PERSONAL phone's text into the first chat of this kind (`direct` or `group`).
    Arrive(&'static str),
}

impl Cue {
    fn request(self) -> Value {
        let (cmd, args) = match self {
            Self::Click => ("add_account", json!({"network": NETWORK_NAME})),
            Self::Arrive(kind) => ("rehearsal_arrive", json!({"kind": kind})),
        };
        json!({"op": "ctl", "cmd": cmd, "args": args, "timeout_ms": 30_000})
    }
}

/// What the walk's user does during one step: his y or n per question, a cue after an answer, a cue
/// after a line the step printed (each once).
#[derive(Clone, Debug, Default)]
pub(super) struct Plan {
    pub answers: Vec<(&'static str, bool)>,
    pub after_answer: Vec<(&'static str, Cue)>,
    pub after_line: Vec<(&'static str, Cue)>,
}

impl Plan {
    pub(super) fn answer_for(&self, question: &str) -> Option<bool> {
        self.answers
            .iter()
            .find(|(q, _)| *q == question)
            .map(|(_, yes)| *yes)
    }
}

/// What one step's call did, as the walk saw it.
#[derive(Debug)]
pub(super) struct Walked {
    pub status: i32,
    pub printed: String,
    /// The questions he answered, in order, with the answer.
    pub answered: Vec<(String, bool)>,
}

// ---- The tool's commands as separate processes ----------------------------------------------------------------

/// Runs this tool's own commands as separate processes, as the user would, each in `unshare -rn`.
pub(super) struct Tool {
    pub exe: PathBuf,
    /// Everything the commands and the terminals printed, searched for leaks at the end.
    pub transcript: String,
}

impl Tool {
    /// `unshare -rn <xtask> threema-run <args> [--rehearsal-place <place>]`.
    pub fn command(&self, args: &[&str], place: Option<&Path>) -> Command {
        let mut cmd = Command::new("unshare");
        cmd.arg("-rn").arg(&self.exe).arg("threema-run").args(args);
        if let Some(place) = place {
            cmd.arg("--rehearsal-place").arg(place);
        }
        // The app's build is up to date; if it were not, nothing could be fetched.
        cmd.env("CARGO_NET_OFFLINE", "true").stdin(Stdio::null());
        lock::die_with_parent(&mut cmd);
        cmd
    }

    fn keep(&mut self, text: &str) {
        self.transcript.push_str(text);
        if !text.ends_with('\n') {
            self.transcript.push('\n');
        }
    }

    /// Runs `cmd`: its exit status and what it printed (both streams), echoed line by line; the child
    /// is killed at `bound`.
    pub fn capture(&mut self, cmd: Command, shown: &str, bound: Duration) -> Result<(i32, String)> {
        println!("\n$ {shown}");
        let (mut child, rx) = spawn_lines(cmd)?;
        let deadline = Instant::now() + bound;
        let mut printed = String::new();
        loop {
            match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(line) => {
                    println!("  | {line}");
                    printed.push_str(&line);
                    printed.push('\n');
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    self.keep(&printed);
                    bail!("`{shown}` did not end in time");
                }
            }
        }
        let status = child.wait()?;
        self.keep(&printed);
        Ok((status.code().unwrap_or(-1), printed))
    }

    /// `threema-run <args>` on `place`.
    fn run(&mut self, place: &Path, args: &[&str]) -> Result<(i32, String)> {
        let cmd = self.command(args, Some(place));
        self.capture(
            cmd,
            &format!("cargo xtask threema-run {}", args.join(" ")),
            CHILD_BOUND,
        )
    }

    /// The command of `threema-run <args>` on a KEPT profile name below `xdg` (a scratch directory
    /// standing for `XDG_DATA_HOME`), with none of the variables that excuse a real start: the refusal
    /// that comes must be the one under test.
    fn kept_command(&self, args: &[&str], xdg: &Path) -> Command {
        let mut cmd = self.command(args, None);
        cmd.env("XDG_DATA_HOME", xdg);
        for name in TEST_ENV {
            cmd.env_remove(name);
        }
        cmd
    }

    /// `id` or `answer` on a terminal of the walk's own, typing `typing` (see [`run_on_a_terminal`]).
    /// What the terminal showed and what the command printed is kept for the leak search.
    fn on_a_terminal(
        &mut self,
        place: &Path,
        args: &[&str],
        typing: &[(&str, &str)],
    ) -> Result<OnATerminal> {
        println!(
            "\n$ (on a terminal) cargo xtask threema-run {}",
            args.join(" ")
        );
        let cmd = self.command(args, Some(place));
        let done = run_on_a_terminal(cmd, typing, Duration::from_secs(60))?;
        for line in done.stdout.lines().chain(done.stderr.lines()) {
            println!("  | {line}");
        }
        self.keep(&done.screen);
        self.keep(&done.stdout);
        self.keep(&done.stderr);
        Ok(done)
    }

    /// His answer to a question, typed into `answer` on a terminal: one key.
    fn answer(&mut self, place: &Path, yes: bool) -> Result<()> {
        let key = if yes { "y" } else { "n" };
        let done = self.on_a_terminal(place, &["answer"], &[(ANSWER_PROMPT_TAIL, key)])?;
        let want = if yes { "taken (yes)" } else { "taken (no)" };
        if done.code != Some(0) || !done.stdout.contains(want) {
            bail!(
                "`answer` did not take the {} key (exit {:?}): {}",
                if yes { "y" } else { "n" },
                done.code,
                done.stderr.trim()
            );
        }
        Ok(())
    }

    /// The ID, typed twice at the prompts of `id`, nothing shown. The terminal, the output and the
    /// error stream must not hold what was typed.
    fn type_the_id(&mut self, place: &Path) -> Result<()> {
        let id = constant("OWN_ID");
        let first = format!("{id}\n");
        let done = self.on_a_terminal(
            place,
            &["id"],
            &[
                (first_prompt_tail(), first.as_str()),
                (PROMPT_AGAIN, first.as_str()),
            ],
        )?;
        for (what, text) in [
            ("terminal", &done.screen),
            ("stdout", &done.stdout),
            ("stderr", &done.stderr),
        ] {
            if text.contains(id) {
                bail!("the {what} of `id` held the ID that was typed");
            }
        }
        if done.code != Some(0) || !done.stdout.contains(TAKEN) {
            bail!(
                "`id` did not take the ID (exit {:?}): {}",
                done.code,
                done.stderr.trim()
            );
        }
        Ok(())
    }

    /// One step's call: the step runs as a process (in `unshare -rn`, with the rehearsal's short
    /// timing), and the walk is the user: it reads what the step prints, answers each question on a
    /// terminal as `plan` says and gives the cues. Returns when the step ended.
    fn walk_step(
        &mut self,
        place: &Path,
        step: &str,
        extra: &[&str],
        plan: &Plan,
    ) -> Result<Walked> {
        let mut args = vec!["step", step];
        args.extend_from_slice(extra);
        let shown = format!("cargo xtask threema-run {}", args.join(" "));
        println!("\n$ {shown}");
        let socket = socket_of(place)?;
        let (mut child, rx) = spawn_lines(self.command(&args, Some(place)))?;
        let deadline = Instant::now() + CHILD_BOUND;
        let mut printed = String::new();
        let mut answered: Vec<(String, bool)> = Vec::new();
        let mut line_cues = vec![false; plan.after_line.len()];
        let mut last_look = Instant::now() - Duration::from_secs(1);
        let ended = loop {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(line) => {
                    println!("  | {line}");
                    printed.push_str(&line);
                    printed.push('\n');
                    for (i, (prefix, cue)) in plan.after_line.iter().enumerate() {
                        if !line_cues[i] && line.starts_with(prefix) {
                            line_cues[i] = true;
                            give(&socket, *cue)?;
                            println!("  (walk) cue after a `{prefix}` line: {cue:?}");
                        }
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break true,
            }
            if last_look.elapsed() >= Duration::from_millis(100) {
                last_look = Instant::now();
                if let Ok(info) = call_at(&socket, &json!({"op": "info"}), Duration::from_secs(5))
                    && let Some(question) = info["question"].as_str()
                {
                    let yes = plan.answer_for(question).ok_or_else(|| {
                        anyhow!("the step `{step}` asked a question the walk has no answer for: {question}")
                    })?;
                    self.answer(place, yes)?;
                    println!(
                        "  (walk) answered `{question}`: {}",
                        if yes { "y" } else { "n" }
                    );
                    answered.push((question.to_owned(), yes));
                    for (after, cue) in &plan.after_answer {
                        if *after == question {
                            give(&socket, *cue)?;
                            println!("  (walk) cue after `{question}`: {cue:?}");
                        }
                    }
                }
            }
            if Instant::now() >= deadline {
                break false;
            }
        };
        if !ended {
            let _ = child.kill();
            let _ = child.wait();
            self.keep(&printed);
            bail!("`{shown}` did not end in time");
        }
        let status = child.wait()?;
        self.keep(&printed);
        Ok(Walked {
            status: status.code().unwrap_or(-1),
            printed,
            answered,
        })
    }
}

/// The text the first prompt of `id` ends with (its full words are `prompt.rs`'s).
fn first_prompt_tail() -> &'static str {
    PROMPT_FIRST
        .rsplit_once(", ")
        .map_or(PROMPT_FIRST, |(_, tail)| tail)
}

/// Starts `cmd` with both streams piped and read by threads into one channel, a line at a time. The
/// channel closes when both streams have.
fn spawn_lines(mut cmd: Command) -> Result<(Child, mpsc::Receiver<String>)> {
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("run a command of the tool")?;
    let (tx, rx) = mpsc::channel::<String>();
    let out = child.stdout.take().context("stdout")?;
    let err = child.stderr.take().context("stderr")?;
    let tx_err = tx.clone();
    for (pipe, tx) in [
        (Box::new(out) as Box<dyn std::io::Read + Send>, tx),
        (Box::new(err) as Box<dyn std::io::Read + Send>, tx_err),
    ] {
        std::thread::spawn(move || {
            for line in BufReader::new(pipe).split(b'\n').map_while(Result::ok) {
                if tx
                    .send(String::from_utf8_lossy(&line).into_owned())
                    .is_err()
                {
                    break;
                }
            }
        });
    }
    Ok((child, rx))
}

/// The control socket of the walk's profile, as its supervisor recorded it.
fn socket_of(place: &Path) -> Result<PathBuf> {
    let dir = ProfileDir {
        dir: place.to_path_buf(),
        rehearsal: true,
    };
    socket_for_client(&dir, &|_| None)
}

/// How long after the app's start the window has put its first page up: the window's own `start()`
/// clears the page stack and pushes the welcome page a moment after the dev channel answers, so a click
/// that came before it would have its link page cleared (and the login cancelled with it). A person
/// clicks seconds later; the walk waits until the window says it is loaded and visible, and until this
/// long after the app's start.
const WINDOW_UP_AFTER_MS: u64 = 6_000;

/// Waits until the window can be clicked (see [`WINDOW_UP_AFTER_MS`]).
fn the_window_is_up(socket: &Path) -> Result<()> {
    let state_says_up = || {
        let answer = call_at(
            socket,
            &json!({"op": "ctl", "cmd": "state", "args": {}, "timeout_ms": 10_000}),
            Duration::from_secs(20),
        )
        .ok()?;
        (answer["result"]["loaded"] == json!(true)
            && answer["result"]["window_visible"] == json!(true))
        .then_some(())
    };
    if wait_for(WAIT_BOUND, state_says_up).is_none() {
        bail!("the window never said it was loaded and visible");
    }
    let started = call_at(socket, &json!({"op": "info"}), Duration::from_secs(10))?["started_ms"]
        .as_u64()
        .unwrap_or(0);
    let wait = (started + WINDOW_UP_AFTER_MS).saturating_sub(unix_now_ms());
    std::thread::sleep(Duration::from_millis(wait));
    Ok(())
}

/// A cue of the fake phone, through the supervisor's socket.
fn give(socket: &Path, cue: Cue) -> Result<()> {
    if cue == Cue::Click {
        the_window_is_up(socket)?;
    }
    call_at(socket, &cue.request(), Duration::from_secs(40))
        .map(|_| ())
        .map_err(|e| anyhow!("the cue {cue:?} was not taken: {e:#}"))
}

/// The supervisor's `info`, or the error.
fn info_of(place: &Path) -> Result<Value> {
    call_at(
        &socket_of(place)?,
        &json!({"op": "info"}),
        Duration::from_secs(10),
    )
}

/// Calls `f` every 100 ms until it returns a value or `bound` passes.
fn wait_for<T>(bound: Duration, mut f: impl FnMut() -> Option<T>) -> Option<T> {
    let deadline = Instant::now() + bound;
    loop {
        if let Some(v) = f() {
            return Some(v);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

// ---- The list of test IDs, watched while it stands ----------------------------------------------------------

/// What the walk saw of the list of test IDs while it stood.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Sighting {
    pub file_mode: u32,
    pub root_mode: u32,
    pub text: String,
}

/// Watches the list's file every millisecond from a thread of its own until stopped; the first sight
/// of it is kept (the app takes it within a moment of the ID, so a watch must already be running when
/// the ID is typed).
struct ListWatch {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<Option<Sighting>>>,
}

impl ListWatch {
    fn start(dir: &ProfileDir) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let (flag, file, root) = (
            Arc::clone(&stop),
            dir.allow_file(),
            dir.keeper().to_path_buf(),
        );
        let handle = std::thread::spawn(move || {
            let mut seen = None;
            while !flag.load(Ordering::Relaxed) {
                if seen.is_none()
                    && let Ok(meta) = std::fs::symlink_metadata(&file)
                    && let Ok(text) = std::fs::read_to_string(&file)
                {
                    seen = Some(Sighting {
                        file_mode: meta.permissions().mode() & 0o7777,
                        root_mode: std::fs::metadata(&root)
                            .map_or(0, |m| m.permissions().mode() & 0o7777),
                        text,
                    });
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            seen
        });
        Self {
            stop,
            handle: Some(handle),
        }
    }

    fn finish(mut self) -> Option<Sighting> {
        self.stop.store(true, Ordering::Relaxed);
        self.handle.take().and_then(|h| h.join().ok()).flatten()
    }
}

impl Drop for ListWatch {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Judges what the watch saw: the list stood, 0600 in a 0700 root, as one line `sha256:` and the
/// 64 digits of `digest`, and nothing else.
pub(super) fn judge_sighting(seen: Option<&Sighting>, digest: &str) -> Result<()> {
    let seen = seen.ok_or_else(|| {
        anyhow!("the list of test IDs was never seen standing (the app took it before the watch looked?)")
    })?;
    if seen.file_mode != 0o600 || seen.root_mode != 0o700 {
        bail!(
            "the list stood with mode {:o} in a keeper root with mode {:o}: it must be 600 in 700",
            seen.file_mode,
            seen.root_mode
        );
    }
    if seen.text != list_text(digest) {
        bail!("the list that stood is not one line `sha256:` and the digest of the typed ID");
    }
    Ok(())
}

// ---- The checks ------------------------------------------------------------------------------------------------

/// What a rehearsal found.
struct Rehearsed {
    passed: u32,
}

/// Numbers a passed check and says so.
fn pass(passed: &mut u32, what: &str) {
    *passed += 1;
    println!("  ok {passed}: {what}");
}

/// A step that must PASS: its exit status 0 and its own `PASS <name>` line.
fn must_pass(w: &Walked, step: &str) -> Result<()> {
    match verdict_of(&w.printed, step) {
        Some(v) if v.pass && w.status == 0 => Ok(()),
        Some(v) => bail!(
            "step {step} ended {} (exit {}): {}",
            if v.pass { "PASS" } else { "FAIL" },
            w.status,
            v.reason
        ),
        None => bail!("step {step} printed no verdict (exit {})", w.status),
    }
}

/// The notes a step printed (`NOTE: ...` lines), for the checks that name one.
fn has_note(printed: &str, prefix: &str) -> bool {
    printed
        .lines()
        .filter_map(|l| l.trim_start().strip_prefix("NOTE: "))
        .any(|n| n.starts_with(prefix))
}

/// The probe by itself, in `unshare -rn`: the three lists of its `networks` line.
fn probe_lists(app: &Path, dir: &ProfileDir) -> Result<(Vec<String>, Vec<String>, Vec<String>)> {
    let inherited: Vec<std::ffi::OsString> = std::env::vars_os().map(|(k, _)| k).collect();
    let cmd = in_unshare(&probe_command(app, dir, true, &inherited));
    let clock = RealClock::new();
    let wait = Wait {
        bound: Duration::from_secs(60),
        poll: Duration::from_millis(50),
        clock: &clock,
    };
    let result = run_bounded(cmd, &wait)?;
    let list = |key: &str| kinds_of(&result.stdout, key);
    match (list("registered="), list("off="), list("on=")) {
        (Some(r), Some(o), Some(n)) => Ok((r, o, n)),
        _ => bail!(
            "the stamped app printed no `networks` line (exit {:?})",
            result.code
        ),
    }
}

/// `wipe` on the wipe place: without its flag nothing goes; with it the profile goes and the keeper
/// root, with the file beside the profile, stays.
/// `backup`'s copy: one directory `<profile>-<time>` below the keeper root's `backups`, 0700, with the
/// profile's state and app data, and no list of test IDs, no stamped program, no file that holds the
/// digest of the list that stood beside it.
fn check_backup(dir: &ProfileDir, list_digest: &str) -> Result<()> {
    let name = dir
        .dir
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    let copies: Vec<PathBuf> = std::fs::read_dir(dir.backups_dir())
        .map(|d| {
            d.flatten()
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name()
                        .is_some_and(|n| n.to_string_lossy().starts_with(&format!("{name}-")))
                })
                .collect()
        })
        .unwrap_or_default();
    let [copy] = copies.as_slice() else {
        bail!(
            "`backup` made {} copies of the profile, not one",
            copies.len()
        );
    };
    if std::fs::metadata(copy).map_or(0, |m| m.permissions().mode() & 0o777) != 0o700 {
        bail!("the backup's directory is not 0700");
    }
    if !copy.join(TOOL_DIR).join("state.json").is_file() || !copy.join("kourier").is_dir() {
        bail!("the backup lacks the profile's state or the app's data");
    }
    if copy.join(TOOL_DIR).join("bin").exists() {
        bail!("the backup holds the stamped program");
    }
    let mut stack = vec![copy.clone()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d)?.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if entry.file_name() == ALLOW_FILE {
                bail!("the backup holds a file named like the list of test IDs");
            }
            let bytes = std::fs::read(&path)?;
            if bytes
                .windows(list_digest.len())
                .any(|w| w == list_digest.as_bytes())
            {
                bail!("a file of the backup holds the digest of the list of test IDs");
            }
        }
    }
    Ok(())
}

fn check_wipe(tool: &mut Tool, wipe: &ProfileDir) -> Result<()> {
    let (code, printed) = tool.run(&wipe.dir, &["wipe"])?;
    if code == 0 || !printed.contains(WIPE_UNDONE) || !wipe.dir.join("a-file").exists() {
        bail!("`wipe` without its flag did not leave the profile alone (exit {code})");
    }
    let (code, printed) = tool.run(&wipe.dir, &["wipe", "--yes-delete-the-login"])?;
    if code != 0 || !printed.contains(WIPE_DONE) {
        bail!("`wipe --yes-delete-the-login` did not delete the profile (exit {code})");
    }
    if wipe.dir.exists() {
        bail!("the profile is still there after `wipe --yes-delete-the-login`");
    }
    if !wipe.keeper().join("keeper-note").exists() {
        bail!("`wipe` took a file of the keeper root with it");
    }
    Ok(())
}

/// The walk.
fn rehearsal(tool: &mut Tool, base: &Path, secs: u64, no_build: bool) -> Result<Rehearsed> {
    let target = target_dir();
    let place = place_under(&target, secs, WALK_DATA, WALK_PROFILE);
    let dir = ProfileDir {
        dir: place.clone(),
        rehearsal: true,
    };
    let wipe = ProfileDir {
        dir: place_under(&target, secs, WIPE_DATA, WIPE_PROFILE),
        rehearsal: true,
    };
    let scratch = base.join(SCRATCH_DATA);
    private_dir(&scratch)?;
    let mut passed = 0;

    // The app is built once, outside any namespace; every `start` below stamps (or reuses) a copy.
    if !no_build {
        real_build()?;
    }
    if !target.join("debug").join("kourier").exists() {
        bail!("no built app at target/debug/kourier: run without --no-build");
    }
    println!(
        "stage:    the app is built; each start stamps a copy of it into the rehearsal's place"
    );

    // (1) `start` without the go on a kept keeper root.
    let cmd = tool.kept_command(&["start", "--for-link", "--profile", "p"], &scratch);
    let (code, printed) = tool.capture(
        cmd,
        "cargo xtask threema-run start --for-link --profile p (no go, a kept keeper root)",
        CHILD_BOUND,
    )?;
    if code == 0 || !printed.contains(GO_REFUSAL) {
        bail!("`start` without the go was not refused with the go's sentence (exit {code})");
    }
    if !is_empty_dir(&scratch) {
        bail!("`start` without the go made something below the scratch keeper root");
    }
    pass(
        &mut passed,
        "start without the user's go is refused and makes nothing",
    );

    // (2) `id` with no run.
    private_dir(dir.keeper())?;
    private_dir(&dir.dir)?;
    let done = tool.on_a_terminal(&place, &["id"], &[])?;
    if done.code == Some(0) || done.screen.contains(first_prompt_tail()) {
        bail!(
            "`id` with no run was not refused before it asked (exit {:?})",
            done.code
        );
    }
    pass(&mut passed, "id with no run is refused and asks nothing");

    // (3) `start --for-link`: no go is asked of a rehearsal, which reaches nothing.
    let (code, printed) = tool.run(&place, &["start", "--for-link"])?;
    if code != 0 || line_with(&printed, "supervisor: up").is_none() {
        bail!("`start --for-link` did not bring a supervisor up (exit {code})");
    }
    if line_with(&printed, "probe:      the app registers Threema").is_none() {
        bail!("the probe did not name Threema as registered");
    }
    if !switch_is_on(&dir) {
        bail!("the switch is not ON after `start --for-link`");
    }
    if dir.allow_file().exists() {
        bail!("a list of test IDs stands before any ID was typed");
    }
    let info = info_of(&place)?;
    if info["id"] != json!("waiting")
        || info["rehearsal"] != json!(true)
        || info["self_dumpable"] != json!(false)
    {
        bail!("the supervisor does not wait for the ID as a rehearsal's, undumpable one: {info}");
    }
    pass(
        &mut passed,
        "start --for-link: the switch is ON, the probe registers Threema, the supervisor waits for the ID and cannot be dumped, no list stands",
    );

    // (4) the ID, the list's whole life, the two refusals.
    let own_digest = digest_of_id(constant("OWN_ID").as_bytes())?;
    let watch = ListWatch::start(&dir);
    tool.type_the_id(&place)?;
    let seen = watch.finish();
    judge_sighting(seen.as_ref(), &own_digest)?;
    let taken = wait_for(WAIT_BOUND, || {
        info_of(&place)
            .ok()
            .filter(|i| i["allow_list"] == json!(LIST_TAKEN) && i["ready"] == json!(true))
    });
    let Some(info) = taken else {
        bail!(
            "the list was not taken at the app's first answer (the supervisor's allow_list is not `{LIST_TAKEN}`)"
        );
    };
    if dir.allow_file().exists() {
        bail!("the list of test IDs stands after the app's first answer");
    }
    if info["id"] != json!("taken") {
        bail!("the supervisor does not say the ID was taken");
    }
    let lock = super::profile::open_lock_file(&dir.keeper_lock_file())?;
    if !super::profile::try_lock_for(&lock, Duration::from_secs(1))? {
        bail!("the keeper lock is still held after the list went");
    }
    drop(lock);
    let again = tool.on_a_terminal(&place, &["id"], &[])?;
    if again.code == Some(0)
        || again.screen.contains(first_prompt_tail())
        || !again.stderr.contains("does not wait for the ID")
    {
        bail!(
            "a second `id` was not refused as one that waits for nothing (exit {:?})",
            again.code
        );
    }
    let none = tool.on_a_terminal(&place, &["answer"], &[])?;
    if none.code == Some(0)
        || none.screen.contains(ANSWER_PROMPT_TAIL)
        || !none.stderr.contains("no question is open")
    {
        bail!(
            "an `answer` with no question open was not refused (exit {:?})",
            none.code
        );
    }
    pass(
        &mut passed,
        "id on a terminal: taken, nothing shown; the list stood 0600 in a 0700 root with the digest's line, was gone at the app's first answer, the keeper lock is free; a second id and an answer with no question are refused",
    );

    // (5) the steps.
    let link_plan = Plan {
        answers: vec![
            ("test-phone-in-hand", true),
            ("emoji-equal", true),
            ("list-shows-kourier", true),
            ("menu-path-as-runbook", true),
            ("phone-warning", false),
        ],
        after_answer: vec![("test-phone-in-hand", Cue::Click)],
        ..Plan::default()
    };
    let link = tool.walk_step(&place, "link", &[], &link_plan)?;
    must_pass(&link, "link")?;
    let asked: Vec<&str> = link.answered.iter().map(|(q, _)| q.as_str()).collect();
    if asked
        != [
            "test-phone-in-hand",
            "emoji-equal",
            "list-shows-kourier",
            "menu-path-as-runbook",
            "phone-warning",
        ]
    {
        bail!("`link` asked the questions {asked:?}, not the five of its order");
    }
    if !has_note(&link.printed, "allow list: removed at the first answer")
        && !has_note(&link.printed, "allow list: taken by the start")
    {
        bail!("`link` does not say the list was taken by the start");
    }
    let quiet = Plan::default();
    must_pass(&tool.walk_step(&place, "sync", &[], &quiet)?, "sync")?;
    let receive = Plan {
        after_line: vec![("PHONE:", Cue::Arrive("direct"))],
        ..Plan::default()
    };
    must_pass(
        &tool.walk_step(&place, "receive", &[], &receive)?,
        "receive",
    )?;
    let group = Plan {
        after_line: vec![("PHONE:", Cue::Arrive("group"))],
        ..Plan::default()
    };
    must_pass(
        &tool.walk_step(&place, "group-receive", &[], &group)?,
        "group-receive",
    )?;
    let passive = tool.walk_step(&place, "passive", &["--seconds", "5"], &quiet)?;
    must_pass(&passive, "passive")?;
    if !has_note(&passive.printed, "frame counts: not measured") {
        bail!(
            "`passive` does not say its frame counts were not measured (the fake prints no count line)"
        );
    }
    // The ends are only on his word: a no to `end-go` changes nothing.
    let no = Plan {
        answers: vec![("end-go", false)],
        ..Plan::default()
    };
    let unlink = tool.walk_step(&place, "unlink", &[], &no)?;
    match verdict_of(&unlink.printed, "unlink") {
        Some(v)
            if !v.pass
                && unlink.status == 1
                && v.reason.contains("do not want the link ended now") => {}
        _ => bail!(
            "`unlink` with a no to end-go did not end at once with nothing changed (exit {})",
            unlink.status
        ),
    }
    let info = info_of(&place)?;
    if info["removal"] != json!(false)
        || info["halted"].is_string()
        || info["online"] != json!(true)
    {
        bail!("a no to end-go left the run changed: {info}");
    }
    pass(
        &mut passed,
        "link (five answers on a terminal, the click, QR, emoji), sync, receive and group-receive (an arrival on cue), passive pass; unlink with a no to end-go changes nothing",
    );

    // (6) state, stop, and the probe after it.
    let (code, printed) = tool.run(&place, &["state"])?;
    if code != 0
        || !printed.contains(&format!("{NETWORK_NAME} is ON in the profile"))
        || !printed.contains("supervisor:  up")
        || !printed.contains("online")
    {
        bail!(
            "`state` of the running profile does not say: switch ON, supervisor up, account online (exit {code})"
        );
    }
    let (code, printed) = tool.run(&place, &["stop"])?;
    if code != 0
        || !printed.contains("supervisor:  ended")
        || !printed.contains("is off in the profile")
        || !printed.contains("app:         not running")
    {
        bail!("`stop` did not end the run with the switch off (exit {code})");
    }
    if switch_is_on(&dir) || dir.allow_file().exists() {
        bail!("after `stop` the switch is ON or a list of test IDs stands");
    }
    let (registered, _off, on) = probe_lists(&dir.copy_file(), &dir)?;
    if registered
        .iter()
        .chain(on.iter())
        .any(|k| k == NETWORK_NAME)
    {
        bail!("the probe registers or switches on Threema with the switch off and no list");
    }
    pass(
        &mut passed,
        "state says the run; stop leaves the switch off, no app, no list; the probe no longer registers Threema",
    );

    // (7) a planted leftover list, a plain start, reconnect.
    let peer_digest = digest_of_id(constant("PEER_ID").as_bytes())?;
    write_private(&dir.allow_file(), &list_text(&peer_digest))?;
    // `backup` of the stopped profile, with that list standing in the keeper root: the copy holds the
    // profile and never the list or the program.
    let (code, printed) = tool.run(&place, &["backup"])?;
    if code != 0 || !printed.contains("backup:") || !printed.contains("backups directory") {
        bail!("`backup` of the stopped profile did not copy it (exit {code})");
    }
    check_backup(&dir, &peer_digest)?;
    let (code, printed) = tool.run(&place, &["start", "--no-build"])?;
    if code != 0
        || line_with(
            &printed,
            "next:       cargo xtask threema-run step reconnect",
        )
        .is_none()
    {
        bail!("the plain `start` did not bring the linked profile up (exit {code})");
    }
    if !printed.contains(&format!(
        "a list of test IDs ({ALLOW_FILE}) was left by a run that did not end cleanly"
    )) {
        bail!("the start did not warn about the planted list");
    }
    if dir.allow_file().exists() {
        bail!("the planted list of test IDs still stands after the start");
    }
    let refused = tool.on_a_terminal(&place, &["id"], &[])?;
    if refused.code == Some(0) || refused.screen.contains(first_prompt_tail()) {
        bail!("`id` was not refused on a start that does not wait for the ID");
    }
    let reconnect = tool.walk_step(&place, "reconnect", &[], &quiet)?;
    must_pass(&reconnect, "reconnect")?;
    if !has_note(&reconnect.printed, "online again without a login step") {
        bail!("`reconnect` does not say it came back without a login step");
    }
    let (code, printed) = tool.run(&place, &["stop"])?;
    if code != 0 || !printed.contains("app:         not running") || switch_is_on(&dir) {
        bail!("the second `stop` did not end the run with the switch off (exit {code})");
    }
    pass(
        &mut passed,
        "backup copies the stopped profile (0700, state and data in it, no list, no program); a planted leftover list is removed by the next start with its warning, that start refuses id, reconnect finds the chats as they were left, stop leaves the switch off",
    );

    // (8) report and wipe.
    let (code, printed) = tool.run(&place, &["report"])?;
    if code != 0 || !printed.contains("step calls") {
        bail!("`report` did not write its file (exit {code})");
    }
    if std::fs::metadata(dir.report_file()).map_or(0, |m| m.permissions().mode() & 0o777) != 0o600 {
        bail!("the report's file is missing or is not 0600");
    }
    ensure(&wipe, &State::new(unix_now_ms()))?;
    std::fs::write(wipe.dir.join("a-file"), "x")?;
    std::fs::write(wipe.keeper().join("keeper-note"), "x")?;
    check_wipe(tool, &wipe)?;
    pass(
        &mut passed,
        "report writes its file (0600); wipe without its flag deletes nothing, with it the profile only",
    );

    // (9) a run with a TEST phone: the same tool, the questions about the TEST phone answered by
    // checks on the scripted phone; the ID is read from it; nothing is typed but the emoji's answer
    // (the fake Threema's dev state carries no emoji for the step to compare).
    let phone_dir = ProfileDir {
        dir: place_under(&target, secs, PHONE_DATA, WALK_PROFILE),
        rehearsal: true,
    };
    let pplace = phone_dir.dir.clone();
    private_dir(phone_dir.keeper())?;
    private_dir(&phone_dir.dir)?;
    let (code, printed) = tool.run(
        &pplace,
        &["start", "--for-link", "--test-phone", PHONE_SERIAL],
    )?;
    if code != 0 || line_with(&printed, "supervisor: up").is_none() {
        bail!("`start --for-link --test-phone` did not bring a supervisor up (exit {code})");
    }
    if !printed.contains(&format!("test phone: {PHONE_SERIAL}")) {
        bail!("the start does not say the profile has a test phone");
    }
    let info = info_of(&pplace)?;
    if info["test_phone"] != json!(true) || info["id"] != json!("waiting") {
        bail!("the supervisor does not report a test phone and wait for the ID: {info}");
    }
    let (code, printed) = tool.run(&pplace, &["id", "--from-test-phone"])?;
    if code != 0 || !printed.contains("read from the TEST phone") {
        bail!("`id --from-test-phone` did not take the ID from the scripted phone (exit {code})");
    }
    let taken = wait_for(WAIT_BOUND, || {
        info_of(&pplace)
            .ok()
            .filter(|i| i["allow_list"] == json!(LIST_TAKEN) && i["ready"] == json!(true))
    });
    if taken.is_none() || phone_dir.allow_file().exists() {
        bail!("the list was not taken and removed after `id --from-test-phone`");
    }
    // The same flag in a run without a test phone is refused (the first walk's profile is stopped; a
    // fresh refusal is proved by the unit tests of the operation).
    let link = Plan {
        answers: vec![("emoji-equal", true), ("phone-warning", false)],
        after_line: vec![("WINDOW:", Cue::Click)],
        ..Plan::default()
    };
    let walked = tool.walk_step(&pplace, "link", &[], &link)?;
    must_pass(&walked, "link")?;
    let asked: Vec<&str> = walked.answered.iter().map(|(q, _)| q.as_str()).collect();
    if asked != ["emoji-equal", "phone-warning"] {
        bail!(
            "`link` with a test phone asked {asked:?}: only the emoji's question and the personal phone's half of the warning are left to a person"
        );
    }
    for needle in [
        "ACT:",
        "answer to \"test-phone-in-hand\": checked on the TEST phone",
        "answer to \"list-shows-kourier\": checked on the TEST phone",
        "answer to \"menu-path-as-runbook\": checked on the TEST phone",
        "answer to \"phone-warning (TEST phone half)\": checked on the TEST phone",
        "answer to \"phone-warning\": typed by the user",
        "answer to \"emoji-equal\": typed by the user",
    ] {
        // ACT: is in no part of `link` (it does acts only in other steps)
        if needle != "ACT:" && !walked.printed.contains(needle) {
            bail!("`link` with a test phone does not name an origin it must: {needle}");
        }
    }
    if !walked.printed.contains("ASK-USER [") {
        bail!("`link` with a test phone did not put the personal phone's half as ASK-USER");
    }
    let quiet = Plan::default();
    must_pass(&tool.walk_step(&pplace, "sync", &[], &quiet)?, "sync")?;
    let receive = Plan {
        after_line: vec![("DO-USER:", Cue::Arrive("direct"))],
        ..Plan::default()
    };
    must_pass(
        &tool.walk_step(&pplace, "receive", &[], &receive)?,
        "receive",
    )?;
    let group = Plan {
        after_line: vec![("DO-USER:", Cue::Arrive("group"))],
        ..Plan::default()
    };
    must_pass(
        &tool.walk_step(&pplace, "group-receive", &[], &group)?,
        "group-receive",
    )?;
    must_pass(
        &tool.walk_step(&pplace, "passive", &["--seconds", "5"], &quiet)?,
        "passive",
    )?;
    // The end needs the flag the lead passes after the user's word: without it nothing changes.
    let unlink = tool.walk_step(&pplace, "unlink", &[], &quiet)?;
    match verdict_of(&unlink.printed, "unlink") {
        Some(v) if !v.pass && v.reason.contains("--user-said-yes") => {}
        _ => bail!(
            "`unlink` with a test phone and no --user-said-yes was not refused with the flag's name"
        ),
    }
    let (code, printed) = tool.run(&pplace, &["state"])?;
    if code != 0 || !printed.contains(&format!("test phone:  {PHONE_SERIAL} (reachable)")) {
        bail!("`state` does not say `test phone: <serial> (reachable)` (exit {code})");
    }
    // A relayed answer with no question open is refused, and asks for no terminal.
    let (code, printed) = tool.run(
        &pplace,
        &["answer", "--relayed", "y", "--question", "pp-delivered"],
    )?;
    if code == 0 || !printed.contains("no question is open") {
        bail!("`answer --relayed y` with nothing asked was not refused (exit {code})");
    }
    let (code, printed) = tool.run(&pplace, &["stop"])?;
    if code != 0 || !printed.contains("is off in the profile") {
        bail!(
            "`stop` of the test-phone walk did not end the run with the switch off (exit {code})"
        );
    }
    pass(
        &mut passed,
        "a run with a TEST phone (the scripted fake): the ID is read from the phone and only its digest goes on; link answers the TEST phone's questions by checks and names each origin; the emoji (typed in his terminal) and the personal phone's half of the warning (ASK-USER) stay questions; sync, receive, group-receive and passive pass; unlink needs --user-said-yes; state names the phone; a relayed answer with no question is refused",
    );

    // (10) the leak search.
    let needles = needles()?;
    let mut found: Vec<String> = Vec::new();
    for root in [
        base.join(WALK_DATA),
        base.join(WIPE_DATA),
        base.join(PHONE_DATA),
        scratch.clone(),
    ] {
        for (file, label) in leaks_in(&root, &needles) {
            found.push(format!("{file}: {label}"));
        }
    }
    for label in hits(tool.transcript.as_bytes(), &needles) {
        found.push(format!("the transcript: {label}"));
    }
    if holds_a_digest(&tool.transcript) {
        found.push("the transcript: a run of 64 hex digits (a digest?)".to_owned());
    }
    if !found.is_empty() {
        bail!(
            "the rehearsal's data stands where nothing may name it: {}",
            found.join("; ")
        );
    }
    let log = std::fs::read_to_string(dir.app_log()).unwrap_or_default();
    let (ids, digests) = redaction_markers(&log);
    println!(
        "  (the supervisor's redaction left {ids} [id] and {digests} [digest] marker(s) in app.log)"
    );
    pass(
        &mut passed,
        "no ID, no digest, no key, no name, no text of the fake in any file (the stamped program aside) or in anything the commands and terminals printed; no digest-shaped run in the tool's files",
    );

    Ok(Rehearsed { passed })
}

/// Ends whatever the walk left running (a failed walk): a supervisor and its app on the walk's place.
fn end_the_run(tool: &mut Tool, place: &Path) {
    if let Ok(socket) = socket_of(place)
        && call_at(&socket, &json!({"op": "info"}), Duration::from_secs(3)).is_ok()
    {
        let _ = tool.run(place, &["stop", "--force"]);
    }
}

/// What a walk does not show, said in its last line (C8).
pub(super) const NOT_SHOWN: &str = "not measured (rehearsal): flight-mode, phone-sleep, kinds (the fake prints no leader line, no D1 and no count line); not shown: mirror (the fake has no act that writes an own message of the phone), removed-on-phone and unlink's removal (the dev control has no remove command), the wrong-ID refusal (the fake has no ID refusal; the backend's tests prove B1's gate), the real join and warning text, the window, and a search of the app's raw stderr (the supervisor redacts before it writes); with a test phone the walk shows link, sync, receive, group-receive, passive and the unlink flag on the scripted phone, and the acts and checks of mirror, phone-sleep, flight-mode (the firewall cut), kinds and the ends are proved only by the unit tests on that phone, never on a device";

/// `cargo xtask threema-run rehearse`: the whole tool against the fake playing Threema, in a throwaway
/// place below `target/thr/`, every command in its own `unshare -rn`. It takes the test lock (a
/// rehearsal is a test run; a real run is not one) and builds the app first, once.
pub(super) fn rehearse(a: &Args) -> Result<i32> {
    let started = Instant::now();
    if !unshare_works() {
        bail!(
            "the rehearsal needs `unshare -rn` (user namespaces) and refuses to run without it: it skips the user's go because it can reach nothing. Nothing was started."
        );
    }
    check_slice(
        &std::fs::read_to_string("/proc/self/cgroup").unwrap_or_default(),
        std::env::var("container").ok().as_deref(),
        true,
    )?;
    lock::take(false)?;
    let target = target_dir();
    let secs = unix_now_ms() / 1000;
    let base = target.join(REHEARSAL_DIR).join(secs.to_string());
    private_dir(&base)?;
    let mut tool = Tool {
        exe: std::env::current_exe().context("the xtask binary")?,
        transcript: String::new(),
    };
    let place = place_under(&target, secs, WALK_DATA, WALK_PROFILE);
    let result = rehearsal(&mut tool, &base, secs, a.no_build);
    end_the_run(&mut tool, &place);
    // The control socket's directory (a RAM place of its own).
    let _ = std::fs::remove_dir_all(
        Path::new(super::launch::REHEARSAL_SOCKET_BASE)
            .join(format!("kthr-{}-{secs}", socket_uid())),
    );
    match result {
        Ok(done) => {
            println!(
                "\nREHEARSAL PASS: {} checks in {} s\n{NOT_SHOWN}",
                done.passed,
                started.elapsed().as_secs()
            );
            if a.keep {
                println!(
                    "kept: {} ({} MiB)",
                    base.strip_prefix(&target).unwrap_or(&base).display(),
                    dir_size(&base) / (1024 * 1024)
                );
            } else if base.starts_with(target.join(REHEARSAL_DIR)) {
                let _ = std::fs::remove_dir_all(&base);
            }
            Ok(0)
        }
        Err(e) => {
            println!(
                "\nREHEARSAL FAIL: {e:#}\nkept for a look: {}",
                base.strip_prefix(&target).unwrap_or(&base).display()
            );
            Ok(1)
        }
    }
}

/// The uid the rehearsal's children see in their user namespace (`unshare -r`: root), which names the
/// socket directory (`kthr-<uid>-<secs>`).
fn socket_uid() -> u32 {
    0
}

/// Whether this machine lets a process unshare a user and a network namespace: the rehearsal never runs
/// without (no network at all is what makes it safe to skip the user's go; wa_run.rs:5867
/// `unshare_works`).
fn unshare_works() -> bool {
    Command::new("unshare")
        .args(["-rn", "true"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

// ---- Doors for the tests ----------------------------------------------------------------------------

#[cfg(test)]
pub(super) fn line_with_for_tests<'a>(printed: &'a str, prefix: &str) -> Option<&'a str> {
    line_with(printed, prefix)
}

#[cfg(test)]
pub(super) fn cue_request_for_tests(cue: Cue) -> Value {
    cue.request()
}
