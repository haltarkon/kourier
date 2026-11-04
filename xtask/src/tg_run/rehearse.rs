//! `cargo xtask tg-run rehearse`: the whole tool against the fake that plays Telegram, with no network.
//!
//! The rehearsal takes the test lock, builds the app ONCE outside any namespace (cargo inside
//! `unshare -rn` changes its fingerprints and rebuilds both ways), stamps that build into a throwaway
//! place below `<target>/tgr/<secs>/x/kourier-tg-run/p` and then runs the tool's own commands as separate
//! processes, EACH in its own `unshare -rn` (so whatever the app or a bug does, nothing can be reached;
//! the child refuses to run anywhere with a network interface, `check_rehearsal_offline`). Nothing in it
//! is Telegram's: `KOURIER_DEV_REHEARSAL=telegram` puts the scripted fake in the network's place, no
//! helper, library or API file exists, and the real dialer is never made.
//!
//! Its checks, each one `passed += 1`:
//!
//! 1. `connect` without the user's go on a kept keeper root (a scratch one) is refused and makes nothing;
//! 2. a connect whose order is NOT confirmed: the app's dialer says `mode=refusing reason=no-confirmation`,
//!    the walk saw no confirmation file (`confirmation: none written`), no nonce was handed over, the
//!    verdict is PASS with the counts "not measured (rehearsal)" and the preflight says `disk not
//!    checked`;
//! 3. a CONFIRMED connect: `mode=telegram reason=confirmed`, the walk saw the file gone BEFORE its own
//!    removal (`confirmation: taken by the app`: the app unlinks its own), the launch description names
//!    the dial variable and no value;
//! 4. in both walks nothing of the real backend ran: no gate-counts line, no lock-down line, no line
//!    about the helper, no API file anywhere in the scratch keeper roots, no helper or library staged;
//! 5. the staged app's probe named Telegram in `registered=` with the fake switch and did NOT without it
//!    (so the fake is what registered it);
//! 6. the first QR step came and no step after it, no account, the page change answered, the state file
//!    holds the two verdicts;
//! 7. `wipe` without its flag deletes nothing, with it deletes the profile and leaves the keeper root;
//! 8. the leak search over every file of the places, the tool's whole transcript and the app's RAW stderr
//!    (each walk counted the needles in every line before its redaction, `raw needles in the app's
//!    stderr: 0`): no constant of the fake, no nonce, no hash of it, no sentinel tail.

use std::ffi::OsStr;
use std::io::{BufRead as _, BufReader, Read as _};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;

use super::app_names::DIAL_ENV;
use super::connect::{
    APP_START_MARK, ORDER_CONFIRMED_LINE, ORDER_UNCONFIRMED_LINE, REHEARSAL_RANDOM_ENV, unix_now_ms,
};
use super::preflight::{TEST_ENV, check_slice, sha256_bytes};
use super::profile::{ProfileDir, State, ensure, private_dir};
use super::report::read_log;
use super::stage::{
    PROBE_BOUND, PROBE_POLL, RealClock, SENTINEL_TAIL, StageSeams, Wait, probe_command, real_build,
    run_bounded, stage,
};
use super::{API_FILE, Args, KEEPER_DIR, REHEARSAL_DIR, TOOL_DIR};
use crate::{lock, target_dir};

/// How long one command of the rehearsal may run.
const CHILD_BOUND: Duration = Duration::from_secs(300);
/// The longest a Unix socket path may be (`sun_path`, 108 bytes with the NUL).
pub(super) const SOCKET_PATH_MAX: usize = 107;
/// What the tool leaves of the 107 bytes to a socket the app might make below the profile
/// (`/kourier/run/` and a name): the app of this run makes none (its dev control rides on its pipes, it
/// takes no bus name), and a test pins the rest (critic 4).
pub(super) const APP_PATH_MARGIN: usize = 20;
/// The names of the rehearsal's places below `<target>/tgr/<secs>/`: the data home of the walks'
/// profile, of the wipe's profile and of the scratch keeper root of check 1.
pub(super) const WALK_DATA: &str = "x";
pub(super) const WIPE_DATA: &str = "w";
const SCRATCH_DATA: &str = "k";
pub(super) const WALK_PROFILE: &str = "p";
pub(super) const WIPE_PROFILE: &str = "w";
/// The file that stands for `/dev/urandom` in the confirmed walk.
const RANDOM_FILE: &str = "random-source";
/// What the go check says; check 1 looks for it.
const GO_REFUSAL: &str = "needs the user's go";
/// The sentence a wipe without its flag ends in, and the word of one with it.
const WIPE_UNDONE: &str = "Nothing was deleted";
const WIPE_DONE: &str = "deleted.";

// ---- The fake's constants -----------------------------------------------------------------------------

/// The tool's copy of every string constant of `crates/kourier-fake/src/telegram.rs`, in the file's
/// order: the rehearsal searches everything it wrote and printed for each (X26 reads the file and pins
/// these copies, names and values).
pub(super) const FAKE_CONSTANTS: [(&str, &str); 18] = [
    ("OWN_NAME", "Rehearsal Tgowner"),
    ("PEER_NAME", "Rehearsal Tgpeer"),
    ("GROUP_NAME", "Rehearsal Tggroup"),
    ("OWN_NUMBER", "999000842917"),
    ("PEER_NUMBER", "999000263508"),
    ("OWN_TG_ID", "7041835296"),
    ("PEER_TG_ID", "6129470358"),
    ("GROUP_CHAT_ID", "-4128790356"),
    ("RIGHT_TAIL", "00842917"),
    ("WRONG_TAIL", "73610528"),
    ("DIRECT_WORDS", "direct talk"),
    ("GROUP_WORDS", "group talk"),
    ("ARRIVAL_WORDS", "Rehearsal arrival"),
    ("CODE", "48215"),
    ("PASSWORD", "kettle-rehearsal-pass"),
    ("PASSWORD_HINT", "a kitchen appliance"),
    ("EMAIL_CODE", "730294"),
    ("EMAIL_ADDRESS", "rehearsal-mailbox@example.invalid"),
];

/// One thing no file or line of the rehearsal may hold: a label (what to call a hit; never the value)
/// and the text.
#[derive(Clone, Debug)]
pub(super) struct Needle {
    pub label: String,
    value: String,
}

/// Every needle of a rehearsal: the fake's constants, the sentinel tail, the confirmed walk's nonce and
/// the nonce's hash.
pub(super) fn needles(nonce: &str, nonce_hash: &str) -> Vec<Needle> {
    let mut out: Vec<Needle> = FAKE_CONSTANTS
        .iter()
        .map(|(name, value)| Needle {
            label: format!("the fake's {name}"),
            value: (*value).to_owned(),
        })
        .collect();
    out.push(Needle {
        label: "the sentinel tail".to_owned(),
        value: SENTINEL_TAIL.to_owned(),
    });
    out.push(Needle {
        label: "the confirmation's nonce".to_owned(),
        value: nonce.to_owned(),
    });
    out.push(Needle {
        label: "the hash of the confirmation's nonce".to_owned(),
        value: nonce_hash.to_owned(),
    });
    out
}

/// The texts the rehearsal's walk searches the app's RAW stderr for, line by line, before the tool's
/// redaction (which would hide a nonce the app printed): the same needles as the leak search, with the
/// nonce of THIS walk (an unconfirmed walk has none).
pub(super) fn raw_needles(nonce: Option<&str>) -> Result<Vec<String>> {
    let (nonce, hash) = match nonce {
        Some(n) => (n.to_owned(), sha256_bytes(n.as_bytes())?),
        None => (String::new(), String::new()),
    };
    Ok(needles(&nonce, &hash)
        .into_iter()
        .map(|n| n.value)
        .filter(|v| !v.is_empty())
        .collect())
}

/// A digit-only needle up to this long is matched as a whole number: a log line's timestamp has six
/// digits of microseconds, which spell a five-digit code by chance now and then.
const SHORT_NUMBER: usize = 7;

/// Whether `text` holds `needle`. A short digit-only needle (a code) counts only where no digit stands
/// beside it; every other needle (a name, a long number, a hash, the sentinel tail, which is the tail of
/// a number) counts anywhere.
pub(super) fn holds(text: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let whole = needle.len() <= SHORT_NUMBER && needle.bytes().all(|b| b.is_ascii_digit());
    let bytes = text.as_bytes();
    let mut from = 0;
    while let Some(at) = text[from..].find(needle) {
        let at = from + at;
        let end = at + needle.len();
        if !whole {
            return true;
        }
        let digit_before = at > 0 && bytes[at - 1].is_ascii_digit();
        let digit_after = bytes.get(end).is_some_and(u8::is_ascii_digit);
        if !digit_before && !digit_after {
            return true;
        }
        // The needle is ASCII digits, so one byte on is a character boundary.
        from = at + 1;
    }
    false
}

/// The labels of the needles that stand in `bytes`.
pub(super) fn hits(bytes: &[u8], needles: &[Needle]) -> Vec<String> {
    let text = String::from_utf8_lossy(bytes);
    needles
        .iter()
        .filter(|n| holds(&text, &n.value))
        .map(|n| n.label.clone())
        .collect()
}

/// Every file below `dir` searched with [`hits`] (the stamped program of the profile aside: it is the
/// program, and holds the fake's names as code): `(path relative to dir, label)`. A file that cannot be
/// read is a finding of its own.
pub(super) fn leaks_in(dir: &Path, needles: &[Needle]) -> Vec<(String, String)> {
    fn walk(
        at: &Path,
        root: &Path,
        needles: &[Needle],
        skip: &Path,
        out: &mut Vec<(String, String)>,
    ) {
        let Ok(entries) = std::fs::read_dir(at) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let relative = || {
                path.strip_prefix(root)
                    .unwrap_or(&path)
                    .display()
                    .to_string()
            };
            if kind.is_dir() {
                if !path.ends_with(skip) {
                    walk(&path, root, needles, skip, out);
                }
            } else if kind.is_file() {
                match std::fs::read(&path) {
                    Ok(bytes) => {
                        for label in hits(&bytes, needles) {
                            out.push((relative(), label));
                        }
                    }
                    Err(_) => out.push((relative(), "a file the search could not read".to_owned())),
                }
            }
        }
    }
    let mut out = Vec::new();
    // `tg-run/bin` is the stamped program.
    walk(
        dir,
        dir,
        needles,
        &Path::new(TOOL_DIR).join("bin"),
        &mut out,
    );
    out
}

// ---- The places -----------------------------------------------------------------------------------------

/// A profile directory of the rehearsal: `<target>/tgr/<secs>/<data>/kourier-tg-run/<name>`. The data
/// home (`<data>`) is the app's `XDG_DATA_HOME`, so its own run-profile rule holds in the rehearsal too.
pub(super) fn place_under(target: &Path, secs: u64, data: &str, name: &str) -> PathBuf {
    target
        .join(REHEARSAL_DIR)
        .join(secs.to_string())
        .join(data)
        .join(KEEPER_DIR)
        .join(name)
}

/// Whether a place fits a Unix socket path with room for the app's own (critic 4).
pub(super) fn fits_a_socket(place: &Path) -> bool {
    place.as_os_str().len() + APP_PATH_MARGIN <= SOCKET_PATH_MAX
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

// ---- Judging what the children printed -----------------------------------------------------------------

/// Whether the connect the rehearsal runs has its order confirmed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Order {
    Unconfirmed,
    Confirmed,
}

/// What a passing walk told.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Walked {
    /// Milliseconds from the app's start to its first QR step.
    pub qr_after_ms: u64,
    /// The app's memory as the report said it ("123 MiB (peak 130 MiB)"), when it was measured.
    pub memory: Option<String>,
}

/// The first line of `printed` that starts (after its blanks) with `prefix`.
fn line_with<'a>(printed: &'a str, prefix: &str) -> Option<&'a str> {
    printed
        .lines()
        .map(str::trim_start)
        .find(|l| l.starts_with(prefix))
}

/// A run of exactly 64 hex digits (a nonce, or a hash of one).
fn has_a_64_hex_run(text: &str) -> bool {
    let b = text.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i].is_ascii_hexdigit() && (i == 0 || !b[i - 1].is_ascii_hexdigit()) {
            let run = b[i..].iter().take_while(|c| c.is_ascii_hexdigit()).count();
            if run == 64 {
                return true;
            }
            i += run;
        } else {
            i += 1;
        }
    }
    false
}

/// Judges what a rehearsal walk printed (checks 2, 3, 4 and 6 over the tool's own words): PASS; the
/// preflight's "disk not checked" (a rehearsal never looked); the dialer line of the order; the counts
/// and the lock-down "not measured (rehearsal)"; the probe line; the order line; the dial variable's NAME
/// in the launch description for a confirmed order and not for the other, never a value; who took the
/// confirmation, as the walk SAW it before its own removal (`taken by the app` for a confirmed order,
/// `none written` for the other); the first QR code and no step after it, no account, the page change
/// answered; the app quit by itself; no needle of the walk in the app's raw stderr; `sends: not
/// measured`.
pub(super) fn judge_walk(printed: &str, order: Order) -> Result<Walked> {
    let confirmed = order == Order::Confirmed;
    if line_with(printed, "connect: PASS").is_none() {
        bail!("the walk did not say `connect: PASS`");
    }
    if let Some(failure) = line_with(printed, "FAILED:") {
        bail!("the walk reports a failure: {failure}");
    }
    let (mode, reason) = super::report::expected_dialer(confirmed);
    let dialer = format!("dialer: mode={mode} reason={reason}");
    if line_with(printed, "dialer:") != Some(dialer.as_str()) {
        bail!(
            "the dialer line is not `{dialer}` ({} order)",
            if confirmed {
                "a confirmed"
            } else {
                "an unconfirmed"
            }
        );
    }
    let preflight = line_with(printed, "preflight:")
        .ok_or_else(|| anyhow!("the walk printed no preflight line"))?;
    if !preflight.contains("; disk not checked;") {
        bail!(
            "the preflight line does not say `disk not checked`: a rehearsal never looks at a disk"
        );
    }
    for needle in [
        "lock-down: not measured (rehearsal)",
        "gate counts: not measured (rehearsal)",
    ] {
        if line_with(printed, needle).is_none() {
            bail!("the walk does not say `{needle}`");
        }
    }
    if line_with(printed, "gate counts (").is_some() {
        bail!("a gate counts line stands in a rehearsal's report: the real backend ran");
    }
    if line_with(printed, "probe:    the staged app registers Telegram").is_none() {
        bail!("the probe did not name Telegram as registered");
    }
    let order_line = if confirmed {
        ORDER_CONFIRMED_LINE
    } else {
        ORDER_UNCONFIRMED_LINE
    };
    if line_with(printed, "order:") != Some(order_line) {
        bail!("the order line is not the one of {order:?}");
    }
    let start =
        line_with(printed, "start:").ok_or_else(|| anyhow!("the walk printed no start line"))?;
    if start.contains(DIAL_ENV) != confirmed {
        bail!(
            "the launch description {} the dial variable",
            if confirmed { "does not name" } else { "names" }
        );
    }
    if has_a_64_hex_run(start) {
        bail!("the launch description holds a 64-digit hex value (a nonce?)");
    }
    let qr = line_with(printed, "first QR step: after ")
        .ok_or_else(|| anyhow!("no first QR step in the walk's report"))?;
    let rest = qr.strip_prefix("first QR step: after ").unwrap_or_default();
    let qr_after_ms = rest
        .split(" ms")
        .next()
        .and_then(|n| n.parse::<u64>().ok())
        .ok_or_else(|| anyhow!("the first QR step has no time"))?;
    if !rest
        .ends_with("; steps after it: 0; accounts after the close: 0; the page change: answered ok")
    {
        bail!(
            "the QR step was not closed cleanly (steps after it, an account, or the page change)"
        );
    }
    let taken = if confirmed {
        "confirmation: taken by the app"
    } else {
        "confirmation: none written"
    };
    if line_with(printed, "confirmation:") != Some(taken) {
        bail!("the walk does not say `{taken}`");
    }
    let raw = line_with(printed, "raw needles in the app's stderr: ").ok_or_else(|| {
        anyhow!("the walk does not say how many needles stood in the app's stderr")
    })?;
    match raw
        .strip_prefix("raw needles in the app's stderr: ")
        .and_then(|n| n.parse::<u64>().ok())
    {
        Some(0) => {}
        Some(n) => bail!(
            "{n} needle(s) of the walk stood in the app's raw stderr (before the tool's redaction)"
        ),
        None => bail!("the walk's count of needles in the app's stderr is not a number"),
    }
    let app = line_with(printed, "the app: quit by itself (status 0)")
        .ok_or_else(|| anyhow!("the app did not quit by itself with status 0"))?;
    let memory = app
        .split("; memory ")
        .nth(1)
        .filter(|m| !m.starts_with("not measured"))
        .map(str::to_owned);
    if line_with(printed, "sends: not measured").is_none() {
        bail!("the report does not say `sends: not measured`");
    }
    Ok(Walked {
        qr_after_ms,
        memory,
    })
}

/// The kinds the probe's `networks registered=<kinds> …` line names as registered; `None` when the
/// output holds no such line.
pub(super) fn registered_in(stdout: &str) -> Option<Vec<String>> {
    let line = stdout.lines().find(|l| l.starts_with("networks "))?;
    let list = line
        .split_whitespace()
        .find_map(|w| w.strip_prefix("registered="))
        .unwrap_or_default();
    Some(
        list.split(',')
            .filter(|k| !k.is_empty())
            .map(str::to_owned)
            .collect(),
    )
}

/// `cmd` run as `unshare -rn <cmd>`: the same program, arguments and environment changes (the probe the
/// rehearsal runs by itself must see no network either).
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

// ---- The tool's commands as separate processes ----------------------------------------------------------

/// Runs this tool's own commands as separate processes, as the lead would, each in `unshare -rn`.
pub(super) struct Tool {
    pub exe: PathBuf,
    /// Everything the commands printed, searched for leaks at the end.
    pub transcript: String,
}

impl Tool {
    /// `unshare -rn <xtask> tg-run <args> [--rehearsal-place <place>]`.
    pub fn command(&self, args: &[&str], place: Option<&Path>) -> Command {
        let mut cmd = Command::new("unshare");
        cmd.arg("-rn").arg(&self.exe).arg("tg-run").args(args);
        if let Some(place) = place {
            cmd.arg("--rehearsal-place").arg(place);
        }
        // The app's build is up to date; if it were not, nothing could be fetched.
        cmd.env("CARGO_NET_OFFLINE", "true").stdin(Stdio::null());
        lock::die_with_parent(&mut cmd);
        cmd
    }

    /// Runs `cmd`: its exit status and what it printed (both streams), echoed line by line; the child
    /// is killed at `bound`.
    pub fn capture(
        &mut self,
        mut cmd: Command,
        shown: &str,
        bound: Duration,
    ) -> Result<(i32, String)> {
        println!("\n$ {shown}");
        let mut child = cmd
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("run a command of the tool")?;
        let (tx, rx) = mpsc::channel::<String>();
        let out = child.stdout.take().context("stdout")?;
        let err = child.stderr.take().context("stderr")?;
        let tx_err = tx.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(out).split(b'\n').map_while(Result::ok) {
                if tx
                    .send(String::from_utf8_lossy(&line).into_owned())
                    .is_err()
                {
                    break;
                }
            }
        });
        std::thread::spawn(move || {
            for line in BufReader::new(err).split(b'\n').map_while(Result::ok) {
                if tx_err
                    .send(String::from_utf8_lossy(&line).into_owned())
                    .is_err()
                {
                    break;
                }
            }
        });
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
                    bail!("`{shown}` did not end in time");
                }
            }
        }
        let status = child.wait()?;
        self.transcript.push_str(&printed);
        Ok((status.code().unwrap_or(-1), printed))
    }

    /// `tg-run <args>` on `place`, with `envs` added.
    fn run(
        &mut self,
        place: &Path,
        args: &[&str],
        envs: &[(&str, &OsStr)],
    ) -> Result<(i32, String)> {
        let mut cmd = self.command(args, Some(place));
        for (key, value) in envs {
            cmd.env(key, value);
        }
        self.capture(
            cmd,
            &format!("cargo xtask tg-run {}", args.join(" ")),
            CHILD_BOUND,
        )
    }

    /// The command of `tg-run <args>` on a KEPT profile name below `xdg` (a scratch directory standing
    /// for `XDG_DATA_HOME`), with none of the variables that excuse a real start: the refusal that
    /// comes must be the one under test.
    pub fn kept_command(&self, args: &[&str], xdg: &Path) -> Command {
        let mut cmd = self.command(args, None);
        cmd.env("XDG_DATA_HOME", xdg);
        for name in TEST_ENV {
            cmd.env_remove(name);
        }
        cmd
    }

    fn run_kept(&mut self, args: &[&str], xdg: &Path) -> Result<(i32, String)> {
        let cmd = self.kept_command(args, xdg);
        self.capture(
            cmd,
            &format!("cargo xtask tg-run {}", args.join(" ")),
            CHILD_BOUND,
        )
    }

    /// A walk that must PASS: its exit status 0 and the words of [`judge_walk`].
    fn walk(&mut self, place: &Path, order: Order, envs: &[(&str, &OsStr)]) -> Result<Walked> {
        let mut args = vec!["connect", "--env", "test", "--no-build"];
        if order == Order::Confirmed {
            args.push("--confirmed-by-user");
        }
        let (code, printed) = self.run(place, &args, envs)?;
        if code != 0 {
            bail!("the {order:?} connect failed (exit {code})");
        }
        judge_walk(&printed, order).with_context(|| format!("the {order:?} connect"))
    }
}

// ---- The checks -----------------------------------------------------------------------------------------

/// What a rehearsal found.
struct Rehearsed {
    passed: u32,
    memory: Option<String>,
}

/// Numbers a passed check and says so.
fn pass(passed: &mut u32, what: &str) {
    *passed += 1;
    println!("  ok {passed}: {what}");
}

/// Whether `dir` holds no entry at all.
fn is_empty_dir(dir: &Path) -> bool {
    std::fs::read_dir(dir).is_ok_and(|mut d| d.next().is_none())
}

/// `wipe` on the wipe place: without its flag nothing goes; with it the profile goes and the keeper
/// root, with the file beside the profile, stays.
fn check_wipe(tool: &mut Tool, wipe: &ProfileDir) -> Result<()> {
    let (code, printed) = tool.run(&wipe.dir, &["wipe"], &[])?;
    if code == 0 || !printed.contains(WIPE_UNDONE) || !wipe.dir.join("a-file").exists() {
        bail!("`wipe` without its flag did not leave the profile alone (exit {code})");
    }
    let (code, printed) = tool.run(&wipe.dir, &["wipe", "--yes-delete-the-login"], &[])?;
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

/// The nonce a rehearsal's random source holds: 32 bytes of `/dev/urandom` written to `file`, and the
/// same bytes as the 64 hex digits the tool will make of them.
pub(super) fn make_random_source(file: &Path) -> Result<String> {
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .context("open the random source")?
        .read_exact(&mut bytes)?;
    {
        use std::io::Write as _;
        let mut f = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(file)
            .context("make the rehearsal's random source")?;
        f.write_all(&bytes)?;
    }
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// The probe by itself, in `unshare -rn`: the kinds it registers with the switch of the rehearsal's fake
/// and without it.
fn probe_kinds(app: &Path, place: &ProfileDir, with_fake: bool) -> Result<Vec<String>> {
    let home = std::fs::canonicalize(&place.dir)?;
    let data_home = super::connect::data_home_of(&home)?;
    let inherited: Vec<std::ffi::OsString> = std::env::vars_os().map(|(k, _)| k).collect();
    let cmd = in_unshare(&probe_command(
        app, &home, &data_home, with_fake, &inherited,
    ));
    let clock = RealClock(Instant::now());
    let wait = Wait {
        bound: PROBE_BOUND,
        poll: PROBE_POLL,
        clock: &clock,
    };
    let result = run_bounded(cmd, &wait)?;
    registered_in(&result.stdout).ok_or_else(|| {
        anyhow!(
            "the staged app printed no `networks` line (exit {:?})",
            result.code
        )
    })
}

fn rehearsal(tool: &mut Tool, base: &Path, secs: u64, no_build: bool) -> Result<Rehearsed> {
    let target = target_dir();
    let place = place_under(&target, secs, WALK_DATA, WALK_PROFILE);
    let wipe_place = place_under(&target, secs, WIPE_DATA, WIPE_PROFILE);
    for p in [&place, &wipe_place] {
        if !fits_a_socket(p) {
            bail!(
                "the rehearsal's place is too long for a socket path ({} bytes + {APP_PATH_MARGIN} > {SOCKET_PATH_MAX}): run from a checkout with a shorter path",
                p.as_os_str().len()
            );
        }
    }
    let dir = ProfileDir {
        dir: place.clone(),
        rehearsal: true,
    };
    let wipe = ProfileDir {
        dir: wipe_place,
        rehearsal: true,
    };
    let scratch = base.join(SCRATCH_DATA);
    private_dir(&scratch)?;
    let mut passed = 0;

    // The stage, once, outside any namespace: the app is built, copied and stamped; the walks below only
    // check the stamp (`--no-build`).
    ensure(&dir, &State::new("test"))?;
    let debug = target.join("debug");
    let skip_build = |_: bool| Ok(());
    let builder: &dyn Fn(bool) -> Result<()> = if no_build { &skip_build } else { &real_build };
    let seams = StageSeams {
        build: builder,
        app: debug.join("kourier"),
        helper: debug.join("kourier-tdlib"),
        library: None,
        head_now: &|| Err(anyhow!("a rehearsal has no main to compare with")),
        now_ms: unix_now_ms(),
    };
    stage(&dir, None, false, &seams)?;
    println!("stage:    the app is built and stamped into the rehearsal's place");

    // (1) a connect without the go on a kept keeper root.
    let (code, printed) =
        tool.run_kept(&["connect", "--env", "test", "--profile", "p"], &scratch)?;
    if code == 0 || !printed.contains(GO_REFUSAL) {
        bail!("`connect` without the go was not refused with the go's sentence (exit {code})");
    }
    if !is_empty_dir(&scratch) {
        bail!("`connect` without the go made something below the scratch keeper root");
    }
    pass(
        &mut passed,
        "connect without the user's go is refused and makes nothing",
    );

    // (2) an unconfirmed order.
    let unconfirmed = tool.walk(&place, Order::Unconfirmed, &[])?;
    pass(
        &mut passed,
        "an unconfirmed connect: the app refuses (no-confirmation), the walk saw no file (none written), PASS, counts not measured",
    );

    // (3) a confirmed order, its nonce known to this process alone for the leak search.
    let random = base.join(RANDOM_FILE);
    let nonce = make_random_source(&random)?;
    let nonce_hash = sha256_bytes(nonce.as_bytes())?;
    let confirmed = tool.walk(
        &place,
        Order::Confirmed,
        &[(REHEARSAL_RANDOM_ENV, random.as_os_str())],
    )?;
    pass(
        &mut passed,
        "a confirmed connect: the app dials as telegram (confirmed) and took the file itself (the walk saw it gone before its own removal), the launch names the variable and no value",
    );

    // (4) nothing of the real backend ran.
    let log = std::fs::read_to_string(dir.app_log()).unwrap_or_default();
    let facts = read_log(&log);
    if !facts.counts.is_empty()
        || facts.counts_unreadable > 0
        || !facts.lockdown.is_empty()
        || facts.helper_lines > 0
    {
        bail!("the app's log holds a gate counts, lock-down or helper line: the real backend ran");
    }
    // The app's own words, walk by walk (the lines after each start mark): the dialer of the first
    // refused, the dialer of the second dialled as the order said.
    let walks: Vec<&str> = log.split(APP_START_MARK).skip(1).collect();
    let dialers: Vec<Vec<(String, String)>> = walks.iter().map(|w| read_log(w).dialer).collect();
    let want = |mode: &str, reason: &str| vec![(mode.to_owned(), reason.to_owned())];
    if dialers
        != [
            want("refusing", "no-confirmation"),
            want("telegram", "confirmed"),
        ]
    {
        bail!(
            "the app's log does not hold the dialer line of each walk, the one its order asked for"
        );
    }
    let keeper = dir.keeper();
    for forbidden in [
        keeper.join(API_FILE),
        scratch.join(KEEPER_DIR),
        dir.helper_copy(),
        dir.tdlib_dir(),
    ] {
        if forbidden.exists() {
            bail!(
                "something stands in the rehearsal that a rehearsal never makes (an API file, a keeper root of the scratch, a helper or a library)"
            );
        }
    }
    pass(
        &mut passed,
        "nothing of the real backend ran: no counts, no lock-down, no helper line, no API file, no helper or library",
    );

    // (5) the probe: the fake registered Telegram, and nothing else would have.
    let with = probe_kinds(&dir.app_copy(), &dir, true)?;
    if !with.iter().any(|k| k == "Telegram") {
        bail!("the probe with the fake's switch does not register Telegram");
    }
    let without = probe_kinds(&dir.app_copy(), &dir, false)?;
    if without.iter().any(|k| k == "Telegram") {
        bail!("the probe WITHOUT the fake's switch registers Telegram: the control failed");
    }
    pass(
        &mut passed,
        "the probe registers Telegram with the fake's switch and not without it",
    );

    // (6) the QR step, the close, the state.
    println!(
        "  (the first QR step came {} ms and {} ms after the app's start)",
        unconfirmed.qr_after_ms, confirmed.qr_after_ms
    );
    let state: Value = serde_json::from_str(&std::fs::read_to_string(dir.state_file())?)?;
    let verdicts: Vec<&str> = state["connects"]
        .as_array()
        .map(|l| l.iter().filter_map(|c| c["verdict"].as_str()).collect())
        .unwrap_or_default();
    if verdicts != ["PASS", "PASS"] || state["env"] != "test" {
        bail!("the state file does not hold the two PASS verdicts of the two walks");
    }
    pass(
        &mut passed,
        "each walk: the first QR step, no step after it, no account, the page change answered, the state keeps the verdicts",
    );

    // (7) wipe, on a place of its own (the leak search below needs the walks' place).
    ensure(&wipe, &State::new("test"))?;
    std::fs::write(wipe.dir.join("a-file"), "x")?;
    std::fs::write(wipe.keeper().join("keeper-note"), "x")?;
    check_wipe(tool, &wipe)?;
    pass(
        &mut passed,
        "wipe without its flag deletes nothing; with it, the profile only",
    );

    // (8) the leak search.
    let needles = needles(&nonce, &nonce_hash);
    let mut found: Vec<String> = Vec::new();
    for root in [base.join(WALK_DATA), scratch.clone()] {
        for (file, label) in leaks_in(&root, &needles) {
            found.push(format!("{file}: {label}"));
        }
    }
    for label in hits(tool.transcript.as_bytes(), &needles) {
        found.push(format!("the transcript: {label}"));
    }
    if !found.is_empty() {
        bail!(
            "the rehearsal's data stands where nothing may name it: {}",
            found.join("; ")
        );
    }
    pass(
        &mut passed,
        "no constant of the fake, no nonce, no hash of it, no sentinel tail in any file or line, none in the app's raw stderr",
    );

    Ok(Rehearsed {
        passed,
        memory: confirmed.memory,
    })
}

/// `cargo xtask tg-run rehearse`: the whole tool against the fake playing Telegram, in a throwaway place
/// below `target/tgr/`, every command in its own `unshare -rn`. It takes the test lock (a rehearsal is a
/// test run; a real run is not one) and builds the app first, once.
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
    let result = rehearsal(&mut tool, &base, secs, a.no_build);
    match result {
        Ok(done) => {
            println!(
                "\nREHEARSAL PASS: {} checks in {} s; the app's memory at the end: {}",
                done.passed,
                started.elapsed().as_secs(),
                done.memory.as_deref().unwrap_or("not read")
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
