//! The walk of `connect` (D-13): the one confirmed start of the app on a kept profile, its dev-control
//! channel driven over the app's own pipes, ONE login QR code that nobody scans, the page change that
//! closes the login, `state`, `quit`, and the verdict over what the app did and said.
//!
//! The tool starts the staged app DIRECTLY (never through `kcap`, `env`, `timeout` or a shell: each
//! would be a process of its own with the nonce in its `/proc/<pid>/environ`), after it has made ITSELF
//! non-dumpable. The nonce is 32 bytes of `/dev/urandom`; it lives in the tool's memory, in the app's
//! environment (set on the `Command` only for a CONFIRMED order) and, for a moment, in the input of the
//! `sha256sum` process the tool starts to hash it, and nowhere else: the file the app takes holds its
//! SHA-256. A same-user process could read it in that moment, and could forge the whole file anyway; the
//! confirmation stops accidents and stale or replayed files, and that user's go is the rest
//! (`docs/telegram-run.md`).

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::OpenOptions;
use std::io::{BufRead as _, BufReader, Read, Write as _};
use std::os::unix::fs::OpenOptionsExt as _;
use std::os::unix::process::CommandExt as _;
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

use super::Args;
use super::app_names::{DIAL_ENV, REHEARSAL_ENV};
use super::preflight::{Cleared, Git, RealGit, check_source, sha256_bytes};
use super::profile::{ProfileDir, write_private};
use super::rehearse::{holds, raw_needles};
use super::report::{
    Confirmation, ExitHow, LogFacts, Report, RunKind, STEPS_KEPT, Step, Verdict, Walk, lines,
    parse_status_memory, read_log, redact, verdict,
};
use super::stage::{
    Clock, PROBE_BOUND, PROBE_POLL, RealClock, SENTINEL_TAIL, StageSeams, TAIL_ENV, Wait,
    append_redacted, base_env, probe, real_build, stage, switches_set, write_switch,
};
use crate::{root, target_dir};

/// The app's log filter: Kourier's own crates at INFO (the dialer line, the lock-down line and the
/// gate's counts are INFO lines; upstream's lines are filtered by the app itself).
const APP_LOG_FILTER: &str = "info,kourier_telegram=info,kourier_app=info";
/// What the tool writes into `app.log` where one walk begins: the verdict reads only the lines after the
/// latest one.
pub(super) const APP_START_MARK: &str = "==== tg-run: app started";
/// The confirmation file's version (the app reads exactly this one).
const DIAL_VERSION: u64 = 1;
/// The servers a confirmation names.
const DIAL_ENV_VALUE: &str = "test";
/// The line a walk prints about its order: whether a confirmation file was written for this start. The
/// rehearsal reads it (an unconfirmed order never writes one).
pub(super) const ORDER_CONFIRMED_LINE: &str =
    "order:    confirmed; the confirmation file was written for this start";
pub(super) const ORDER_UNCONFIRMED_LINE: &str =
    "order:    not confirmed; no confirmation file was written";
/// The rehearsal's one hook into the nonce: a file of 32 bytes that stands for `/dev/urandom`, named by
/// this variable and honoured ONLY for a rehearsal place (see [`random_source`]), so the rehearsal can
/// look for the nonce in everything the run wrote. A real run ignores it.
pub(super) const REHEARSAL_RANDOM_ENV: &str = "KOURIER_TG_RUN_REHEARSAL_RANDOM";

/// The bounds of the walk. Every wait is on the clock the walk is given.
#[derive(Clone, Copy, Debug)]
pub(super) struct Bounds {
    /// From the app's start to the first `qr` step.
    pub first_qr: Duration,
    /// An answer to a command.
    pub answer: Duration,
    /// From `quit` to the app's end, then SIGKILL.
    pub exit: Duration,
    /// The pipes' end after the app is gone.
    pub drain: Duration,
    /// The step of every wait.
    pub poll: Duration,
}

impl Bounds {
    /// D-13 as changed by the fix round (Q2): <= 180 s for the first QR code, the app's end <= 40 s. The
    /// rehearsal's confirmed walk took 57 s to its QR at a server load of 174 with the fake alone; the
    /// real walk runs on the PC at low CPU weight beside the user's work and adds TDLib's connect. A
    /// timeout fails SAFE but spends the user's go.
    pub const REAL: Self = Self {
        first_qr: Duration::from_secs(180),
        answer: Duration::from_secs(20),
        exit: Duration::from_secs(40),
        drain: Duration::from_secs(5),
        poll: Duration::from_millis(50),
    };
}

// ---- The tool makes itself non-dumpable ------------------------------------------------------------

/// Proof that this process made itself non-dumpable: the nonce is made only with one (wa_run.rs:1799
/// `NonDumpable`).
#[derive(Clone, Copy, Debug)]
pub(super) struct NonDumpable {
    _private: (),
}

/// `prctl(PR_SET_DUMPABLE, 0)`: no core file, no `ptrace`, no `/proc/<pid>/mem` for the user's other
/// processes (wa_run.rs:1806 `make_non_dumpable`).
pub(super) fn make_non_dumpable() -> Result<NonDumpable> {
    // SAFETY: `prctl` with `PR_SET_DUMPABLE` takes plain integers and changes this process's flag only.
    if unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) } != 0 {
        bail!(
            "cannot make this process non-dumpable ({}): the confirmation's nonce would be exposed to a core file. Nothing was started.",
            std::io::Error::last_os_error()
        );
    }
    Ok(NonDumpable { _private: () })
}

// ---- The confirmation -------------------------------------------------------------------------------

/// Whether the order is confirmed: only with `--confirmed-by-user`. A real `connect` has the flag (the
/// preflight refuses without it); the rehearsal's connect may go without it and then must be refused
/// by the app (`mode=refusing reason=no-confirmation`).
pub(super) fn order_confirmed(a: &Args) -> bool {
    a.confirmed
}

/// The nonce: 32 bytes of `reader` (`/dev/urandom`) as 64 lower-case hex digits.
pub(super) fn fresh_nonce(_witness: NonDumpable, reader: &mut dyn Read) -> Result<String> {
    let mut bytes = [0u8; 32];
    reader
        .read_exact(&mut bytes)
        .context("read the nonce from the random source")?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// The text of the confirmation file (D-10): the nonce's hash and never the nonce.
pub(super) fn dial_file_text(issued_ms: u64, nonce: &str) -> Result<String> {
    Ok(format!(
        "{:#}\n",
        json!({
            "version": DIAL_VERSION,
            "issued_ms": issued_ms,
            "env": DIAL_ENV_VALUE,
            "nonce_sha256": sha256_bytes(nonce.as_bytes())?,
        })
    ))
}

/// Writes `tg-run/dial-go.json` (0600) through a temporary file and an atomic rename, right before the
/// app starts.
pub(super) fn write_dial_file(dir: &ProfileDir, issued_ms: u64, nonce: &str) -> Result<()> {
    write_private(&dir.dial_file(), &dial_file_text(issued_ms, nonce)?)
}

/// Removes a confirmation file the app did not take (it unlinks its own at once).
pub(super) fn remove_dial_file(dir: &ProfileDir) {
    let _ = std::fs::remove_file(dir.dial_file());
}

pub(super) fn unix_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

// ---- The app's command ------------------------------------------------------------------------------

/// The command that starts the app: the staged program itself, `--dev-control`, the environment CLEARED of
/// every inherited `KOURIER_*` variable and then exactly these: the profile (canonical), the test
/// servers, the sentinel tail, no QR link in events, no core dump, no desktop services, the log filter,
/// offscreen and silent; the dial variable (`nonce`) ONLY for a confirmed order and the rehearsal switch
/// ONLY in a rehearsal (wa_run.rs:1689 `app_command`, smoke.rs:874). The command holds the nonce in
/// its environment: never print it with `{:?}`, use [`describe_launch`].
pub(super) fn app_command(
    bin: &Path,
    home: &Path,
    data_home: &Path,
    nonce: Option<&str>,
    rehearsal: bool,
    inherited: &[OsString],
) -> Command {
    let mut cmd = Command::new(bin);
    cmd.args(["--dev-control", "--size=1200x800"]);
    base_env(&mut cmd, home, data_home, inherited);
    cmd.env("KOURIER_DEV_KEYRING", "passphrase")
        // A crash must not leave the profile's keys in a core dump.
        .env("KOURIER_DEV_NO_DUMP", "1")
        // The QR code is never shown in an event or a log, and the app runs offscreen: nobody sees it.
        .env("KOURIER_DEV_QR_URL", "0")
        .env(TAIL_ENV, SENTINEL_TAIL)
        // No notifications, no tray, no bus name.
        .env("KOURIER_DESKTOP", "0")
        .env("KOURIER_LOG", APP_LOG_FILTER)
        .env("QT_FORCE_STDERR_LOGGING", "1");
    if rehearsal {
        cmd.env(REHEARSAL_ENV, "telegram");
    }
    if let Some(nonce) = nonce {
        cmd.env(DIAL_ENV, nonce);
    }
    // The app dies with the tool: a tool killed at its time-out must not leave an app behind that
    // holds a login code open.
    // SAFETY: `prctl` is async-signal-safe and only sets this child's own death signal; nothing else
    // runs between fork and exec.
    unsafe {
        cmd.pre_exec(|| {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    cmd
}

/// What the tool will start, for the person to read: the program's file name, its arguments and the
/// NAMES of the variables set for it, never a value (one of them is the nonce) and never a path.
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

// ---- The channel ------------------------------------------------------------------------------------

/// Reads `reader` line by line as bytes (a line that is not UTF-8 must not end the reading: an app whose
/// pipe nobody empties stops at its next write; wa_run.rs:3178 `for_each_line`).
fn for_each_line(reader: impl Read, mut each: impl FnMut(&str)) {
    let mut reader = BufReader::new(reader);
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        each(String::from_utf8_lossy(&line).trim_end_matches(['\n', '\r']));
    }
}

/// What the readers tell the driver.
enum Msg {
    Out(String),
    OutEof,
    ErrEof,
}

/// An `app.log` larger than this stops growing (wa_run.rs:2120 `APP_LOG_KEEP_BYTES`).
const APP_LOG_CAP: u64 = 64 * 1024 * 1024;

/// The driver of the dev-control channel: the events and responses the reader thread sends, the
/// commands written to the app's stdin, every wait bounded on the clock.
struct Driver<'a> {
    stdin: Option<ChildStdin>,
    rx: Receiver<Msg>,
    clock: &'a dyn Clock,
    bounds: &'a Bounds,
    started: u64,
    next_id: u64,
    responses: HashMap<u64, Value>,
    out_eof: bool,
    err_eof: bool,
    status: Option<std::process::ExitStatus>,
    walk: Walk,
    /// The index in `walk.steps` of the last `login_step` when it is a `qr` step of the kind `qr`
    /// still waiting for its `login_qr` event.
    pending_qr: Option<usize>,
    /// A login step of the kind `failed` was seen: nothing more is worth waiting for.
    failed: bool,
}

/// What a wait waits for.
#[derive(Clone, Copy)]
enum Until {
    Qr,
    Response(u64),
    Exit,
    Eof,
}

impl Driver<'_> {
    fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::OutEof => self.out_eof = true,
            Msg::ErrEof => self.err_eof = true,
            Msg::Out(line) => {
                let Ok(v @ Value::Object(_)) = serde_json::from_str::<Value>(&line) else {
                    return;
                };
                match v["type"].as_str() {
                    Some("response") => {
                        if let Some(id) = v["id"].as_u64() {
                            self.responses.insert(id, v);
                        }
                    }
                    Some("event") => self.event(&v),
                    _ => {}
                }
            }
        }
    }

    /// One event of the app. The login steps are kept as (step word, kind word); a QR code counts as
    /// SHOWN only when the `login_qr` event with `shown` follows its step (the app is started with the
    /// link switched off, so a `login_qr` that carries a link is a leak: the flag is set and the value is
    /// never read, kept or printed). An `error` event of a login or an account is counted; its message is
    /// never read.
    fn event(&mut self, v: &Value) {
        let data = &v["data"];
        match v["event"].as_str() {
            Some("login_step") => {
                let step = Step::new(
                    data["step"].as_str().unwrap_or_default(),
                    data["kind"].as_str().unwrap_or_default(),
                );
                self.failed |= step.kind == "failed";
                self.pending_qr = None;
                if self.walk.steps.len() < STEPS_KEPT {
                    if step.is_qr() {
                        self.pending_qr = Some(self.walk.steps.len());
                    }
                    self.walk.steps.push(step);
                } else {
                    // Not kept, so not judged: the verdict fails a walk that dropped any (R-1).
                    self.walk.steps_dropped = self.walk.steps_dropped.saturating_add(1);
                }
            }
            Some("login_qr") => {
                if data.get("url").is_some() {
                    self.walk.qr_link_printed = true;
                }
                if data["shown"] == json!(true)
                    && let Some(at) = self.pending_qr.take()
                {
                    self.walk.steps[at].shown = true;
                    if self.walk.qr_after_ms.is_none() {
                        self.walk.qr_after_ms =
                            Some(self.clock.now_ms().saturating_sub(self.started));
                    }
                }
            }
            Some("error") if matches!(data["source"].as_str(), Some("login" | "account")) => {
                self.walk.errors = self.walk.errors.saturating_add(1);
            }
            _ => {}
        }
    }

    /// Takes the readers' messages until `until` holds or `bound` has passed on the clock. Returns
    /// whether it held. A wait for anything but the app's end stops at once when the app's stdout is
    /// closed: nothing more will come.
    fn pump(&mut self, until: Until, bound: Duration, mut child: Option<&mut Child>) -> bool {
        let start = self.clock.now_ms();
        let bound_ms = u64::try_from(bound.as_millis()).unwrap_or(u64::MAX);
        loop {
            match self.rx.recv_timeout(self.bounds.poll) {
                Ok(msg) => self.handle(msg),
                // A poll that heard nothing: the clock counts it (a test clock moves on only here).
                Err(RecvTimeoutError::Timeout) => self.clock.idle(),
                Err(RecvTimeoutError::Disconnected) => {
                    self.out_eof = true;
                    self.err_eof = true;
                    std::thread::sleep(self.bounds.poll);
                    self.clock.idle();
                }
            }
            let met = match until {
                Until::Qr => self.walk.qr_after_ms.is_some() || self.failed,
                Until::Response(id) => self.responses.contains_key(&id),
                Until::Exit => match child.as_mut().map(|c| c.try_wait()) {
                    Some(Ok(Some(status))) => {
                        self.status = Some(status);
                        true
                    }
                    _ => false,
                },
                Until::Eof => self.out_eof && self.err_eof,
            };
            if met {
                return true;
            }
            if self.out_eof && !matches!(until, Until::Exit | Until::Eof) {
                return false;
            }
            if self.clock.now_ms().saturating_sub(start) >= bound_ms {
                return false;
            }
        }
    }

    /// Writes one command; its id, or `None` when the channel is closed.
    fn send(&mut self, cmd: &str, args: Value) -> Option<u64> {
        self.next_id += 1;
        let id = self.next_id;
        let line = json!({"id": id, "cmd": cmd, "args": args}).to_string();
        let stdin = self.stdin.as_mut()?;
        (writeln!(stdin, "{line}").and_then(|()| stdin.flush())).ok()?;
        Some(id)
    }

    /// One command and its answer, within the answer bound.
    fn request(&mut self, cmd: &str, args: Value) -> Option<Value> {
        let id = self.send(cmd, args)?;
        let bound = self.bounds.answer;
        self.pump(Until::Response(id), bound, None);
        self.responses.remove(&id)
    }
}

/// Starts the app `cmd` and drives it: waits for the first QR code the app shows (or a failed login
/// step: nothing more will come), changes the page to the list
/// (the app cancels a login when its page is left), reads `state`, asks the app to quit, waits for its
/// end (SIGKILL at the bound), and reads its pipes to their end. The app's stderr goes, redacted, into
/// `log` (capped). With `needles` (a rehearsal's list), each RAW stderr line is searched for them before
/// the redaction and the hits are counted into the walk. A program that cannot be started is an error.
/// Everything else is in the [`Walk`].
pub(super) fn drive(
    mut cmd: Command,
    log: &Path,
    bounds: &Bounds,
    clock: &dyn Clock,
    needles: &[String],
) -> Result<Walk> {
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().context("start the app")?;
    let (tx, rx) = channel::<Msg>();
    let stdout = child.stdout.take().context("the app's stdout")?;
    let stderr = child.stderr.take().context("the app's stderr")?;
    let out_tx = tx.clone();
    std::thread::spawn(move || {
        for_each_line(stdout, |line| {
            let _ = out_tx.send(Msg::Out(line.to_owned()));
        });
        let _ = out_tx.send(Msg::OutEof);
    });
    let log = log.to_owned();
    let raw_hits = Arc::new(AtomicU64::new(0));
    let (counted, needles_here) = (Arc::clone(&raw_hits), needles.to_vec());
    std::thread::spawn(move || {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&log)
            .ok();
        let mut written = file
            .as_ref()
            .and_then(|f| f.metadata().ok())
            .map_or(0, |m| m.len());
        for_each_line(stderr, |line| {
            // A rehearsal looks at the RAW line first: the redaction below would hide a nonce the app
            // printed (a run of 64 hex digits) from a search that came after it.
            let hit = needles_here.iter().filter(|n| holds(line, n)).count();
            if hit > 0 {
                counted.fetch_add(hit as u64, Ordering::SeqCst);
            }
            // The pipe is read to its end whether or not the file can be written, and a file at its cap
            // stops growing.
            if written < APP_LOG_CAP
                && let Some(f) = file.as_mut()
            {
                let text = redact(line);
                written += text.len() as u64 + 1;
                let _ = writeln!(f, "{text}");
            }
        });
        let _ = tx.send(Msg::ErrEof);
    });
    let mut d = Driver {
        stdin: child.stdin.take(),
        rx,
        clock,
        bounds,
        started: clock.now_ms(),
        next_id: 0,
        responses: HashMap::new(),
        out_eof: false,
        err_eof: false,
        status: None,
        walk: Walk::default(),
        pending_qr: None,
        failed: false,
    };
    if d.pump(Until::Qr, bounds.first_qr, None) {
        let page = d.request("page", json!({"name": "list"}));
        d.walk.closed = page.map(|v| v["ok"] == json!(true));
        let state = d.request("state", json!({}));
        d.walk.accounts =
            state.and_then(|v| v["result"]["accounts"].as_array().map(|a| a.len() as u64));
    }
    d.walk.memory_kb = std::fs::read_to_string(format!("/proc/{}/status", child.id()))
        .ok()
        .and_then(|text| parse_status_memory(&text));
    let _ = d.request("quit", json!({}));
    // Closing the channel is a second way to say quit.
    d.stdin = None;
    if d.pump(Until::Exit, bounds.exit, Some(&mut child)) {
        d.walk.exit = ExitHow::Quit(d.status.and_then(|s| s.code()).unwrap_or(-1));
    } else {
        let _ = child.kill();
        let _ = child.wait();
        d.walk.exit = ExitHow::Killed;
    }
    d.pump(Until::Eof, bounds.drain, None);
    if !needles.is_empty() {
        d.walk.raw_needles = Some(raw_hits.load(Ordering::SeqCst));
    }
    Ok(d.walk)
}

// ---- The walk of one start --------------------------------------------------------------------------

/// What one walk of the app found.
pub(super) struct Outcome {
    pub walk: Walk,
    pub log: LogFacts,
}

/// What a walk needs.
pub(super) struct Run<'a> {
    pub dir: &'a ProfileDir,
    pub kind: RunKind,
    pub confirmed: bool,
    /// The program to start: the staged app (a test passes a script that plays one).
    pub program: &'a Path,
    pub home: &'a Path,
    pub data_home: &'a Path,
    pub inherited: &'a [OsString],
    pub bounds: &'a Bounds,
    pub clock: &'a dyn Clock,
    pub now_ms: &'a dyn Fn() -> u64,
    /// The random source of the nonce (`/dev/urandom`).
    pub random: &'a dyn Fn() -> Result<Box<dyn Read>>,
}

/// The text of `app.log` after the latest start mark.
fn log_of_last_walk(path: &Path) -> String {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    match text.rfind(APP_START_MARK) {
        Some(at) => text[at..].to_owned(),
        None => String::new(),
    }
}

/// One walk of the app, from the nonce to the log: a leftover confirmation file is removed first; the
/// nonce and the file exist for a CONFIRMED order only; the file is removed after a start that failed
/// and after the walk (the app unlinks its own at once, so this finds nothing when all went well).
pub(super) fn walk_app(run: &Run<'_>, witness: NonDumpable) -> Result<Outcome> {
    let dir = run.dir;
    remove_dial_file(dir);
    let nonce = if run.confirmed {
        let mut source = (run.random)()?;
        let nonce = fresh_nonce(witness, &mut *source)?;
        write_dial_file(dir, (run.now_ms)(), &nonce)?;
        Some(nonce)
    } else {
        None
    };
    println!(
        "{}",
        if nonce.is_some() {
            ORDER_CONFIRMED_LINE
        } else {
            ORDER_UNCONFIRMED_LINE
        }
    );
    let cmd = app_command(
        run.program,
        run.home,
        run.data_home,
        nonce.as_deref(),
        run.kind == RunKind::Rehearsal,
        run.inherited,
    );
    println!("start:    {}", describe_launch(&cmd));
    if std::fs::metadata(dir.app_log()).is_ok_and(|m| m.len() > APP_LOG_CAP) {
        let _ = std::fs::rename(dir.app_log(), dir.app_log().with_file_name("app.log.1"));
    }
    append_redacted(&dir.app_log(), APP_START_MARK, "")?;
    // A rehearsal knows what must never reach the app's stderr; a real run keeps no such list.
    let needles = if run.kind == RunKind::Rehearsal {
        raw_needles(nonce.as_deref())?
    } else {
        Vec::new()
    };
    let mut walk = match drive(cmd, &dir.app_log(), run.bounds, run.clock, &needles) {
        Ok(walk) => walk,
        Err(e) => {
            remove_dial_file(dir);
            return Err(e);
        }
    };
    // Who took the confirmation, looked at BEFORE the tool's own removal below: the app unlinks its own
    // file at once, so a file that still stands was not taken (or was never to exist).
    let stood = std::fs::symlink_metadata(dir.dial_file()).is_ok();
    walk.confirmation = Some(match (nonce.is_some(), stood) {
        (true, false) => Confirmation::TakenByApp,
        (_, true) => Confirmation::Left,
        (false, false) => Confirmation::NoneWritten,
    });
    remove_dial_file(dir);
    let log = read_log(&log_of_last_walk(&dir.app_log()));
    Ok(Outcome { walk, log })
}

// ---- The whole connect ------------------------------------------------------------------------------

/// One `connect` per profile at a time: the lock `wipe` asks for.
pub(super) fn acquire_lock(dir: &ProfileDir) -> Result<std::fs::File> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(dir.lock_file())
        .context("open the profile's lock")?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => {
            bail!("another connect runs on this profile: nothing was started")
        }
        Err(std::fs::TryLockError::Error(e)) => Err(e).context("lock the profile"),
    }
}

/// The real source of the nonce.
fn urandom() -> Result<Box<dyn Read>> {
    Ok(Box::new(
        std::fs::File::open("/dev/urandom").context("open the random source")?,
    ))
}

/// The source of the nonce: `/dev/urandom`; for a REHEARSAL place only, the file `var` names under
/// [`REHEARSAL_RANDOM_ENV`] when it is set (the rehearsal knows the bytes, so it can search for the nonce).
/// A real run never looks at the variable.
pub(super) fn random_source(
    rehearsal: bool,
    var: &dyn Fn(&str) -> Option<String>,
) -> Result<Box<dyn Read>> {
    if rehearsal && let Some(path) = var(REHEARSAL_RANDOM_ENV) {
        return Ok(Box::new(
            std::fs::File::open(path).context("open the rehearsal's random source")?,
        ));
    }
    urandom()
}

/// The keeper root's data home (the app derives the run profile's keeper root from it): the profile's
/// canonical directory's grandparent.
pub(super) fn data_home_of(home: &Path) -> Result<std::path::PathBuf> {
    home.parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .ok_or_else(|| anyhow!("the profile is not below a keeper root"))
}

/// The report's and the state's last words: prints the lines, writes `report.md`, records the verdict.
pub(super) fn finish(
    cleared: &mut Cleared,
    kind: RunKind,
    confirmed: bool,
    outcome: &Outcome,
    verdict: &Verdict,
) -> Result<()> {
    let build = cleared.head.as_deref().map(|h| &h[..h.len().min(10)]);
    let name = cleared.dir.label();
    let report = Report {
        profile: &name,
        env: "test",
        build,
        confirmed,
        kind,
        walk: &outcome.walk,
        log: &outcome.log,
        verdict,
    };
    let text = lines(&report);
    for line in &text {
        println!("{line}");
    }
    write_private(
        &cleared.dir.report_file(),
        &format!("{}\n", text.join("\n")),
    )?;
    cleared.state.record_connect(unix_now_ms(), verdict.word());
    cleared.state.save(&cleared.dir)
}

/// `connect` after the preflight: the lock, the stage, the switch file, the probe, the walk, the
/// verdict. Returns the exit status: 0 for PASS, 1 for FAIL.
pub(super) fn connect(a: &Args, mut cleared: Cleared) -> Result<i32> {
    let rehearsal = a.place.rehearsal_place.is_some();
    let kind = if rehearsal {
        RunKind::Rehearsal
    } else {
        RunKind::Real
    };
    let confirmed = order_confirmed(a);
    let dir = cleared.dir.clone();
    let _lock = acquire_lock(&dir)?;
    // A confirmation an earlier run left is gone before anything is built or started.
    remove_dial_file(&dir);
    let witness = make_non_dumpable()?;
    let clock = RealClock(std::time::Instant::now());
    let inherited: Vec<OsString> = std::env::vars_os().map(|(k, _)| k).collect();

    // The stage.
    let debug = target_dir().join("debug");
    let git = RealGit { root: &root() };
    let head_now = || check_source(&git as &dyn Git);
    let seams = StageSeams {
        build: &real_build,
        app: debug.join("kourier"),
        helper: debug.join("kourier-tdlib"),
        library: cleared.library.clone(),
        head_now: &head_now,
        now_ms: unix_now_ms(),
    };
    stage(&dir, cleared.head.as_deref(), a.no_build, &seams)?;
    println!(
        "stage:    {} staged and stamped (profile {})",
        if rehearsal {
            "the app is"
        } else {
            "the app, its helper and the library are"
        },
        dir.label()
    );

    // The switch file, the probe, the switch file again (the probe reads it; the start clears it).
    let home = std::fs::canonicalize(&dir.dir).context("the profile's canonical directory")?;
    let data_home = data_home_of(&home)?;
    write_switch(&dir)?;
    probe(
        &dir,
        &dir.app_copy(),
        &home,
        &data_home,
        rehearsal,
        &inherited,
        &Wait {
            bound: PROBE_BOUND,
            poll: PROBE_POLL,
            clock: &clock,
        },
    )?;
    println!("probe:    the staged app registers Telegram");
    write_switch(&dir)?;
    if !switches_set(&dir) {
        bail!("the profile's kourier.toml does not carry the switches: nothing was started");
    }

    // The walk.
    let now = unix_now_ms;
    let source = || random_source(rehearsal, &|k| std::env::var(k).ok());
    let run = Run {
        dir: &dir,
        kind,
        confirmed,
        program: &dir.app_copy(),
        home: &home,
        data_home: &data_home,
        inherited: &inherited,
        bounds: &Bounds::REAL,
        clock: &clock,
        now_ms: &now,
        random: &source,
    };
    let outcome = walk_app(&run, witness)?;
    let verdict = verdict_of(&outcome, confirmed, kind);
    finish(&mut cleared, kind, confirmed, &outcome, &verdict)?;
    Ok(i32::from(!verdict.pass))
}

fn verdict_of(outcome: &Outcome, confirmed: bool, kind: RunKind) -> Verdict {
    verdict(&outcome.walk, &outcome.log, confirmed, kind)
}
