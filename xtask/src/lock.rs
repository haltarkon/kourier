//! One test run at a time on this PC (T5a, 2026-10-16; the user's choice "The three small ones",
//! `target/test-load/decisions.md`).
//!
//! `cargo xtask ci` (both tiers) and `cargo xtask shots` take an exclusive `flock` on ONE lock
//! file that both checkouts share (`kourier` and the worktree `kourier-wa`): `kourier-test.lock`
//! in `/run/user/<uid>` when that directory exists, else `.cache/kourier/test.lock` of the home
//! the password database names for the uid (the environment is NOT read: `$XDG_RUNTIME_DIR`,
//! `$XDG_CACHE_HOME` and `$HOME` of a silent run or a systemd scope differ, and two runs would
//! take two files; a file that cannot be opened falls through to the next place); never `/tmp`,
//! which is RAM and may be private to a process. Two
//! runs side by side take 1.5 to 2 times as long and flap (the load flakes of 2026-10-14/15), so
//! a second run WAITS, says once who holds the lock (pid, checkout, command, since when) and,
//! when it has it, how long it waited; `--no-wait` exits 75 instead. The lock is held for the
//! whole run, the prune at its end included, and is gone when the process is: a `flock` is
//! released when the last copy of its descriptor closes, so a stale holder cannot exist.
//!
//! The descriptor is `close-on-exec` (std opens every file so), and that is what lets a test
//! binary, an app a test started or a daemon outlive its parent without keeping the lock:
//! `Command::spawn` copies the descriptor table into the child until its `exec`, and a lock
//! lives as long as ANY copy of the descriptor does (`.claude/rules/testing.md`, the
//! `File::try_lock` trap).
//!
//! `cargo xtask lock [--no-wait] -- <cmd>` runs a command under the same lock, for a load proof
//! of an agent (copies side by side, busy loops) that must not run beside a `ci`. A process the
//! holder starts is told so (`KOURIER_TEST_LOCK_HELD=<pid>`), and a nested `cargo xtask ci`
//! under `cargo xtask lock -- …` (or a `shots` inside `ci`) does not wait for its own parent.

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::fmt;
use std::fs::{File, OpenOptions, TryLockError};
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The exit status of a run that did not wait (`--no-wait`) because the lock was held: sysexits'
/// EX_TEMPFAIL, "try again later".
pub const EXIT_BUSY: i32 = 75;

/// Set in the environment of what the lock holder starts: the holder's pid. A nested
/// `cargo xtask ci`/`shots` that finds its own parent's pid here, and the lock file saying that
/// pid holds the lock, does not take the lock again (it would wait for its parent for ever).
pub const HELD_ENV: &str = "KOURIER_TEST_LOCK_HELD";

/// How long a lock that looks held is asked for again before it counts as held: a program
/// another thread just started carries a copy of the descriptor until its `exec`
/// (`smoke::try_lock_for` has the story).
const SETTLE: Duration = Duration::from_millis(400);

/// The test lock is held by another run and the caller said not to wait. `main` turns it into
/// exit status [`EXIT_BUSY`].
#[derive(Debug)]
pub struct Busy(pub String);

impl fmt::Display for Busy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Busy {}

/// The exit status `main` ends with for this error: [`EXIT_BUSY`] for [`Busy`], else 1.
pub fn exit_code(err: &anyhow::Error) -> i32 {
    if err.downcast_ref::<Busy>().is_some() {
        EXIT_BUSY
    } else {
        1
    }
}

/// Who holds the lock, as the holder wrote it into the lock file after it took the lock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Holder {
    pub pid: u32,
    /// The checkout the run belongs to.
    pub checkout: String,
    /// The `cargo xtask` arguments.
    pub command: String,
    /// Unix seconds when it took the lock.
    pub since: u64,
}

impl Holder {
    fn to_json(&self) -> String {
        json!({
            "pid": self.pid,
            "checkout": self.checkout,
            "command": self.command,
            "since": self.since,
        })
        .to_string()
    }

    fn parse(text: &str) -> Option<Holder> {
        let v: Value = serde_json::from_str(text.trim()).ok()?;
        Some(Holder {
            pid: u32::try_from(v["pid"].as_u64()?).ok()?,
            checkout: v["checkout"].as_str()?.to_owned(),
            command: v["command"].as_str()?.to_owned(),
            since: v["since"].as_u64()?,
        })
    }

    /// "pid 1234 (ci --quick in /home/x/kourier-wa), since 17:02 (12 min ago)".
    pub fn describe(&self, now: u64) -> String {
        format!(
            "pid {} (`cargo xtask {}` in {}), since {} ({} ago)",
            self.pid,
            self.command,
            self.checkout,
            clock_time(self.since),
            span(now.saturating_sub(self.since))
        )
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Local wall-clock time "HH:MM" of a Unix time, as `date +%H:%M` writes it.
pub fn clock_time(unix: u64) -> String {
    let t = unix as libc::time_t;
    // SAFETY: `localtime_r` fills the `tm` it is given and keeps no pointer to it.
    let tm = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&t, &mut tm).is_null() {
            return "??:??".to_owned();
        }
        tm
    };
    format!("{:02}:{:02}", tm.tm_hour, tm.tm_min)
}

/// "40 s", "12 min", "2 h 5 min".
pub fn span(secs: u64) -> String {
    match secs {
        0..=89 => format!("{secs} s"),
        90..=5399 => format!("{} min", (secs + 30) / 60),
        _ => format!("{} h {} min", secs / 3600, (secs % 3600) / 60),
    }
}

/// The places for the lock file, best first: `kourier-test.lock` in `/run/user/<uid>` when that
/// directory exists, then `test.lock` in `.cache/kourier/` of the user's home. Both come from the
/// uid alone (T5 review F2): `$XDG_RUNTIME_DIR`, `$XDG_CACHE_HOME` and `$HOME` are ignored, because
/// a run started with a private runtime directory (an agent's silent run, a systemd scope, another
/// desktop session) would take another file and overlap the run the lock exists to keep apart.
pub fn lock_candidates_in(run_user: &Path, home: Option<&Path>) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if run_user.is_dir() {
        candidates.push(run_user.join("kourier-test.lock"));
    }
    if let Some(home) = home.filter(|h| h.is_absolute()) {
        candidates.push(home.join(".cache/kourier/test.lock"));
    }
    candidates
}

/// The home directory the password database names for `uid` (not `$HOME`).
fn passwd_home(uid: libc::uid_t) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    let mut size = 1024;
    while size <= 64 * 1024 {
        let mut buf: Vec<libc::c_char> = vec![0; size];
        // SAFETY: an all-zero `passwd` is a valid value to be filled in.
        let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
        let mut found: *mut libc::passwd = std::ptr::null_mut();
        // SAFETY: the buffer and the struct outlive the call, `found` receives a pointer into
        // them (or null), and the string is copied out before they are dropped.
        let rc = unsafe { libc::getpwuid_r(uid, &mut pwd, buf.as_mut_ptr(), size, &mut found) };
        if rc == libc::ERANGE {
            size *= 2;
            continue;
        }
        if rc != 0 || found.is_null() || pwd.pw_dir.is_null() {
            return None;
        }
        // SAFETY: `pw_dir` points at a NUL-terminated string inside `buf`.
        let dir = unsafe { std::ffi::CStr::from_ptr(pwd.pw_dir) };
        return Some(PathBuf::from(std::ffi::OsStr::from_bytes(dir.to_bytes())));
    }
    None
}

/// [`lock_candidates_in`] for this process's user.
pub fn lock_candidates() -> Vec<PathBuf> {
    // SAFETY: `getuid` has no preconditions.
    let uid = unsafe { libc::getuid() };
    lock_candidates_in(
        Path::new(&format!("/run/user/{uid}")),
        passwd_home(uid).as_deref(),
    )
}

fn pid_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

/// The holder the lock file names, if it names a process that is alive. The holder writes its
/// line a moment after it took the lock, so a line that is not there yet, or is the previous
/// holder's, is asked for again for half a second.
fn read_holder(path: &Path) -> Option<Holder> {
    for _ in 0..10 {
        if let Some(h) = std::fs::read_to_string(path)
            .ok()
            .and_then(|text| Holder::parse(&text))
            .filter(|h| pid_alive(h.pid))
        {
            return Some(h);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

/// What a run says about itself in the lock file.
#[derive(Debug, Clone)]
pub struct Who {
    pub checkout: String,
    pub command: String,
}

/// A lock that is held: the open file keeps it, closing the file (dropping this, or the process
/// ending however it ends) releases it.
#[derive(Debug)]
pub struct Lock {
    _file: File,
}

fn try_lock_for(file: &File, wait: Duration) -> Result<bool> {
    let deadline = Instant::now() + wait;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(true),
            Err(TryLockError::WouldBlock) => {
                if Instant::now() >= deadline {
                    return Ok(false);
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(TryLockError::Error(e)) => return Err(e).context("try the test lock"),
        }
    }
}

/// Opens (and makes, with its directory) the lock file at `path`.
fn open_lock_file(path: &Path) -> Result<File> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    // std opens with O_CLOEXEC: the lock stays with this process and does not follow a program
    // it starts (the test in this file proves it).
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("open {}", path.display()))
}

/// [`acquire_file`] on the file at `path` (the tests' way in).
#[cfg(test)]
pub fn acquire_at(path: &Path, who: &Who, wait: bool, say: &mut dyn FnMut(&str)) -> Result<Lock> {
    acquire_file(open_lock_file(path)?, path, who, wait, say)
}

/// The first of `paths` whose file opens (a directory that cannot be written, a read-only bind in
/// a sandbox, falls through to the next place), locked as [`acquire_file`] does.
pub fn acquire_first(
    paths: &[PathBuf],
    who: &Who,
    wait: bool,
    say: &mut dyn FnMut(&str),
) -> Result<Lock> {
    let mut failures = Vec::new();
    for path in paths {
        match open_lock_file(path) {
            Ok(file) => return acquire_file(file, path, who, wait, say),
            Err(e) => failures.push(format!("{e:#}")),
        }
    }
    if failures.is_empty() {
        bail!("no place for the test lock: neither /run/user/<uid> nor the home's .cache exists");
    }
    bail!("no place for the test lock: {}", failures.join("; "))
}

/// Takes the exclusive lock of the open file at `path`. When another process holds it: with `wait`
/// it says (once, through `say`) who holds it, waits, and says how long it waited; without `wait`
/// it returns [`Busy`]. The holder's line is written into the file once the lock is ours.
fn acquire_file(
    file: File,
    path: &Path,
    who: &Who,
    wait: bool,
    say: &mut dyn FnMut(&str),
) -> Result<Lock> {
    let started = Instant::now();
    if !try_lock_for(&file, SETTLE)? {
        let holder = match read_holder(path) {
            Some(h) => h.describe(unix_now()),
            None => "another test run (it has not written its details yet)".to_owned(),
        };
        if !wait {
            return Err(Busy(format!(
                "test lock: held by {holder}; --no-wait: not waiting (exit {EXIT_BUSY})"
            ))
            .into());
        }
        say(&format!(
            "test lock: held by {holder}. Waiting for it (--no-wait exits {EXIT_BUSY} instead); {}",
            path.display()
        ));
        loop {
            match file.lock() {
                Ok(()) => break,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e).context("wait for the test lock"),
            }
        }
        say(&format!(
            "test lock: got it after waiting {}",
            span(started.elapsed().as_secs())
        ));
    }
    let line = Holder {
        pid: std::process::id(),
        checkout: who.checkout.clone(),
        command: who.command.clone(),
        since: unix_now(),
    }
    .to_json();
    file.set_len(0)
        .and_then(|()| file.write_all_at(line.as_bytes(), 0))
        .with_context(|| format!("write the holder into {}", path.display()))?;
    Ok(Lock { _file: file })
}

/// This process's lock, kept until the process ends.
static HELD: Mutex<Option<Lock>> = Mutex::new(None);

/// [`take`]'s decision on a slot: the lock is taken once per process and kept in `slot`.
fn take_in(
    slot: &Mutex<Option<Lock>>,
    paths: &[PathBuf],
    who: &Who,
    wait: bool,
    say: &mut dyn FnMut(&str),
) -> Result<()> {
    let mut slot = slot.lock().unwrap_or_else(|p| p.into_inner());
    if slot.is_some() {
        // `ci` runs the screenshot step, which `shots` runs too: one lock, one process.
        return Ok(());
    }
    *slot = Some(acquire_first(paths, who, wait, say)?);
    Ok(())
}

/// Whether this process was started by the process that holds the lock at `path`: the
/// environment names that process, the lock file names it as the holder and it is alive.
fn nested_under(env: Option<&str>, holder: Option<&Holder>, alive: impl Fn(u32) -> bool) -> bool {
    let Some(pid) = env.and_then(|v| v.parse::<u32>().ok()) else {
        return false;
    };
    holder.is_some_and(|h| h.pid == pid) && alive(pid)
}

/// The command line after `cargo xtask`, as the lock file shows it.
fn command_text() -> String {
    let text = std::env::args().skip(1).collect::<Vec<_>>().join(" ");
    text.chars().take(200).collect()
}

/// Takes the test lock for the rest of this process (`ci`, `shots`, `lock`): waits for another
/// run to end, or with `no_wait` fails with [`Busy`]. Does nothing when this process holds it
/// already or was started by the process that does.
pub fn take(no_wait: bool) -> Result<()> {
    let paths = lock_candidates();
    let env = std::env::var(HELD_ENV).ok();
    if env.is_some() {
        let nested = paths.iter().any(|path| {
            let holder = std::fs::read_to_string(path)
                .ok()
                .and_then(|t| Holder::parse(&t));
            nested_under(env.as_deref(), holder.as_ref(), pid_alive)
        });
        if nested {
            return Ok(());
        }
    }
    let who = Who {
        checkout: crate::root().display().to_string(),
        command: command_text(),
    };
    take_in(&HELD, &paths, &who, !no_wait, &mut |m| eprintln!("\n{m}"))
}

/// Whether this process holds the test lock.
fn holds(slot: &Mutex<Option<Lock>>) -> bool {
    slot.lock().unwrap_or_else(|p| p.into_inner()).is_some()
}

fn mark_in(slot: &Mutex<Option<Lock>>, cmd: &mut Command) {
    if holds(slot) {
        cmd.env(HELD_ENV, std::process::id().to_string());
    }
}

/// Tells a program this process starts that it runs under the test lock ([`HELD_ENV`]), when
/// this process holds it.
pub fn mark_children(cmd: &mut Command) {
    mark_in(&HELD, cmd);
}

/// `lock [--no-wait] -- <cmd> [args]`: (no_wait, the command).
fn parse_args(args: &[String]) -> Result<(bool, Vec<String>)> {
    let mut no_wait = false;
    let mut it = args.iter();
    for arg in it.by_ref() {
        match arg.as_str() {
            "--no-wait" => no_wait = true,
            "--" => {
                let cmd: Vec<String> = it.cloned().collect();
                if cmd.is_empty() {
                    bail!("lock: nothing to run after `--`");
                }
                return Ok((no_wait, cmd));
            }
            other => bail!(
                "lock: unknown argument {other:?} (usage: lock [--no-wait] -- <command> [args])"
            ),
        }
    }
    bail!("lock: usage: lock [--no-wait] -- <command> [args]")
}

fn status_code(status: ExitStatus) -> i32 {
    status
        .code()
        .or_else(|| status.signal().map(|s| 128 + s))
        .unwrap_or(1)
}

/// Makes the program `cmd` starts die with this process (`PR_SET_PDEATHSIG` = SIGKILL, T5 review
/// F9/F8): the lock goes with the holder, and a load proof that outlived a killed holder would run
/// on beside the next test run, which is the overlap the lock exists to prevent.
pub fn die_with_parent(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    let parent = std::process::id();
    // SAFETY: only `prctl` and `getppid` run between fork and exec; both are async-signal-safe.
    unsafe {
        cmd.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            // The parent may have died before the call above: nobody will signal then.
            if libc::getppid() as u32 != parent {
                return Err(std::io::Error::other(
                    "the lock holder ended before the start",
                ));
            }
            Ok(())
        });
    }
}

/// The command `lock -- <cmd>` runs: told that it runs under the lock, and dying with the holder.
fn lock_child(cmd: &[String]) -> Command {
    let mut child = Command::new(&cmd[0]);
    child.args(&cmd[1..]);
    mark_children(&mut child);
    die_with_parent(&mut child);
    child
}

/// `cargo xtask lock [--no-wait] -- <cmd> [args]`: runs the command under the test lock and
/// ends with its exit status.
pub fn command(args: &[String]) -> Result<()> {
    let (no_wait, cmd) = parse_args(args)?;
    take(no_wait)?;
    let status = lock_child(&cmd)
        .status()
        .with_context(|| format!("lock: run {}", cmd[0]))?;
    if !status.success() {
        std::process::exit(status_code(status));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::process::Stdio;
    use std::sync::mpsc;

    /// A scratch directory below `target/` (the RAM disk `/tmp` is not for tests).
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = crate::root()
                .join("target/tmp")
                .join(format!("xtask-lock-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("scratch directory");
            Scratch(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn who(command: &str) -> Who {
        Who {
            checkout: "/work/kourier-wa".to_owned(),
            command: command.to_owned(),
        }
    }

    const CHILD: &str = "lock::tests::child_holder";

    /// Not a test: the process the tests below start. They run this test binary again with this
    /// test's name and `KOURIER_LOCK_TEST_PATH` set; it takes the lock, says `locked <pid>`,
    /// optionally starts a long-lived program (`KOURIER_LOCK_TEST_GRANDCHILD`, says
    /// `grandchild <pid>`), waits for the release file and ends. Without the variable it does
    /// nothing (it also runs as an ordinary test).
    #[test]
    // The program it starts must outlive it: that is what the test is about.
    #[allow(clippy::zombie_processes)]
    fn child_holder() {
        let Some(path) = std::env::var_os("KOURIER_LOCK_TEST_PATH") else {
            return;
        };
        let lock = acquire_at(Path::new(&path), &who("ci --quick"), true, &mut |_| {})
            .expect("the child takes the lock");
        // libtest has printed `test <name> ... ` without a newline: start a line of our own.
        println!();
        if let Some(mode) = std::env::var_os("KOURIER_LOCK_TEST_GRANDCHILD") {
            // "die": started the way `cargo xtask lock -- <cmd>` starts its command.
            let mut sleep = if mode == "die" {
                lock_child(&["sleep".to_owned(), "120".to_owned()])
            } else {
                let mut plain = Command::new("sleep");
                plain.arg("120");
                plain
            };
            sleep
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            let grandchild = sleep.spawn().expect("start sleep");
            println!("grandchild {}", grandchild.id());
        }
        println!("locked {}", std::process::id());
        let release = std::env::var_os("KOURIER_LOCK_TEST_RELEASE").expect("release file");
        let give_up = Instant::now() + Duration::from_secs(60);
        while !Path::new(&release).exists() && Instant::now() < give_up {
            std::thread::sleep(Duration::from_millis(20));
        }
        drop(lock);
    }

    /// A running [`CHILD`].
    struct Child {
        process: std::process::Child,
        pid: u32,
        grandchild: Option<u32>,
        release: PathBuf,
    }

    impl Child {
        /// Starts the child and returns when it holds the lock at `lock`.
        fn start(lock: &Path, release: &Path, grandchild: bool) -> Child {
            Child::start_with(lock, release, grandchild.then_some("plain"))
        }

        /// [`Child::start`], with the program the child starts: `"plain"` (as a test starts
        /// an app) or `"die"` (as `cargo xtask lock -- <cmd>` starts its command).
        fn start_with(lock: &Path, release: &Path, grandchild: Option<&str>) -> Child {
            let mut cmd = Command::new(std::env::current_exe().expect("this test binary"));
            cmd.args(["--exact", CHILD, "--nocapture", "--test-threads", "1"])
                .env("KOURIER_LOCK_TEST_PATH", lock)
                .env("KOURIER_LOCK_TEST_RELEASE", release)
                .stdout(Stdio::piped())
                .stderr(Stdio::null());
            if let Some(mode) = grandchild {
                cmd.env("KOURIER_LOCK_TEST_GRANDCHILD", mode);
            }
            let mut process = cmd.spawn().expect("start the child");
            let out = process.stdout.take().expect("piped");
            let (tx, rx) = mpsc::channel();
            std::thread::spawn(move || {
                let mut seen = (None, None);
                // Reads to the end: a closed pipe would fail the child's last line.
                for line in BufReader::new(out).lines().map_while(|l| l.ok()) {
                    if let Some(p) = line.trim().strip_prefix("grandchild ") {
                        seen.1 = p.trim().parse::<u32>().ok();
                    }
                    if let Some(p) = line.trim().strip_prefix("locked ") {
                        seen.0 = p.trim().parse::<u32>().ok();
                        let _ = tx.send(seen);
                    }
                }
            });
            let (pid, grandchild) = rx
                .recv_timeout(Duration::from_secs(60))
                .expect("the child says it holds the lock");
            Child {
                process,
                pid: pid.expect("pid"),
                grandchild,
                release: release.to_owned(),
            }
        }

        fn release(&mut self) {
            std::fs::write(&self.release, "go").expect("release");
        }

        fn wait(&mut self) {
            let status = self.process.wait().expect("child ends");
            assert!(status.success(), "the child failed: {status}");
        }
    }

    impl Drop for Child {
        fn drop(&mut self) {
            let _ = std::fs::write(&self.release, "go");
            let _ = self.process.kill();
            let _ = self.process.wait();
            if let Some(pid) = self.grandchild {
                let _ = Command::new("kill").arg(pid.to_string()).status();
            }
        }
    }

    #[test]
    fn a_second_process_waits_until_the_first_releases() {
        let scratch = Scratch::new("waits");
        let lock = scratch.0.join("kourier-test.lock");
        let release = scratch.0.join("release");
        let mut holder = Child::start(&lock, &release, false);
        let (tx, rx) = mpsc::channel();
        let path = lock.clone();
        let waiter = std::thread::spawn(move || {
            let mut said = Vec::new();
            let got = acquire_at(&path, &who("shots"), true, &mut |m| said.push(m.to_owned()));
            tx.send(()).ok();
            (got.map(drop), said)
        });
        // Longer than the settle and the holder's read, shorter than anything else.
        assert!(
            rx.recv_timeout(Duration::from_millis(1500)).is_err(),
            "the second run took the lock while the first holds it"
        );
        holder.release();
        rx.recv_timeout(Duration::from_secs(60))
            .expect("the second run gets the lock when the first releases");
        let (got, said) = waiter.join().expect("waiter");
        got.expect("lock");
        holder.wait();
        assert_eq!(said.len(), 2, "{said:?}");
        let first = &said[0];
        assert!(
            first.contains(&format!("pid {}", holder.pid))
                && first.contains("cargo xtask ci --quick")
                && first.contains("/work/kourier-wa")
                && first.contains("since "),
            "it names the holder: {first}"
        );
        assert!(said[1].contains("after waiting"), "{}", said[1]);
        // The second run is the holder now.
        let line = std::fs::read_to_string(&lock).expect("holder line");
        let now = Holder::parse(&line).expect("a holder line");
        assert_eq!(now.pid, std::process::id());
        assert_eq!(now.command, "shots");
    }

    #[test]
    fn no_wait_ends_with_status_75_naming_the_holder() {
        let scratch = Scratch::new("nowait");
        let lock = scratch.0.join("kourier-test.lock");
        let release = scratch.0.join("release");
        let mut holder = Child::start(&lock, &release, false);
        let started = Instant::now();
        let err = acquire_at(&lock, &who("ci"), false, &mut |_| {
            panic!("--no-wait must not wait and say it waits")
        })
        .expect_err("the lock is held");
        assert!(started.elapsed() < Duration::from_secs(20));
        assert!(err.downcast_ref::<Busy>().is_some(), "{err:#}");
        assert_eq!(exit_code(&err), 75);
        assert_eq!(EXIT_BUSY, 75);
        let text = err.to_string();
        assert!(text.contains(&format!("pid {}", holder.pid)), "{text}");
        assert!(text.contains("cargo xtask ci --quick"), "{text}");
        // Another failure is not "busy".
        assert_eq!(exit_code(&anyhow::anyhow!("something else")), 1);
        holder.release();
        holder.wait();
        // Free again: --no-wait takes it.
        acquire_at(&lock, &who("ci"), false, &mut |_| {}).expect("free after the holder ended");
    }

    /// The point of `O_CLOEXEC`: a program the holder starts (a test's app, a daemon) outlives
    /// the holder without keeping the lock. Without it the lock stays until that program ends.
    #[test]
    fn programs_the_holder_started_do_not_hold_the_lock_after_it_ends() {
        let scratch = Scratch::new("cloexec");
        let lock = scratch.0.join("kourier-test.lock");
        let release = scratch.0.join("release");
        let mut holder = Child::start(&lock, &release, true);
        let grandchild = holder.grandchild.expect("the child started a program");
        holder.release();
        holder.wait();
        assert!(
            pid_alive(grandchild),
            "the program the holder started must still run for this test to mean something"
        );
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock)
            .expect("open");
        assert!(
            try_lock_for(&file, Duration::from_secs(5)).expect("ask"),
            "the lock stays held by pid {grandchild}, a program the ended holder started"
        );
    }

    #[test]
    fn a_run_that_holds_the_lock_does_not_ask_for_it_again() {
        // `ci` runs the screenshot step, which `shots` runs too: the same process must not wait
        // for itself (a second open of the file is another description and would conflict).
        let scratch = Scratch::new("once");
        let lock = scratch.0.join("kourier-test.lock");
        let slot = Mutex::new(None);
        let paths = [lock];
        take_in(&slot, &paths, &who("ci"), false, &mut |_| {}).expect("first");
        assert!(holds(&slot));
        take_in(&slot, &paths, &who("ci"), false, &mut |_| {
            panic!("a run that holds the lock says nothing")
        })
        .expect("the second take of the same process is a no-op");
        // And what it starts is told, so a nested `cargo xtask` does not wait for it either.
        let mut cmd = Command::new("true");
        mark_in(&slot, &mut cmd);
        let env: Vec<_> = cmd.get_envs().collect();
        assert_eq!(
            env,
            [(
                std::ffi::OsStr::new(HELD_ENV),
                Some(std::ffi::OsStr::new(&std::process::id().to_string()))
            )]
        );
        let mut other = Command::new("true");
        mark_in(&Mutex::new(None), &mut other);
        assert_eq!(other.get_envs().count(), 0, "only the holder marks");
    }

    #[test]
    fn a_nested_run_knows_its_parent_holds_the_lock() {
        let holder = Holder {
            pid: 4242,
            checkout: "/c".into(),
            command: "ci".into(),
            since: 1,
        };
        let alive = |pid: u32| pid == 4242;
        assert!(nested_under(Some("4242"), Some(&holder), alive));
        // No variable, a variable of another process, a holder that is gone, junk.
        assert!(!nested_under(None, Some(&holder), alive));
        assert!(!nested_under(Some("77"), Some(&holder), |_| true));
        assert!(!nested_under(Some("4242"), Some(&holder), |_| false));
        assert!(!nested_under(Some("4242"), None, alive));
        assert!(!nested_under(Some("not a pid"), Some(&holder), alive));
    }

    #[test]
    fn where_the_lock_file_is() {
        let scratch = Scratch::new("path");
        let run = scratch.0.join("run");
        std::fs::create_dir_all(&run).expect("dir");
        let missing = Path::new("/nonexistent/run/user/1");
        // /run/user/<uid> when it exists, then the home's cache; nothing else, never /tmp.
        assert_eq!(
            lock_candidates_in(&run, Some(Path::new("/h"))),
            [
                run.join("kourier-test.lock"),
                PathBuf::from("/h/.cache/kourier/test.lock")
            ]
        );
        assert_eq!(
            lock_candidates_in(missing, Some(Path::new("/h"))),
            [PathBuf::from("/h/.cache/kourier/test.lock")]
        );
        assert_eq!(
            lock_candidates_in(&run, None),
            [run.join("kourier-test.lock")]
        );
        // A relative home is nobody's: both checkouts must name the same file.
        assert_eq!(
            lock_candidates_in(missing, Some(Path::new("home"))),
            Vec::<PathBuf>::new()
        );
        let real = lock_candidates();
        assert!(!real.is_empty(), "this session has a place");
        assert!(real.iter().all(|p| !p.starts_with("/tmp")), "{real:?}");
        // The first one is the session's runtime directory of this uid (the machine has one).
        // SAFETY: `getuid` has no preconditions.
        let uid = unsafe { libc::getuid() };
        if Path::new(&format!("/run/user/{uid}")).is_dir() {
            assert_eq!(
                real[0],
                PathBuf::from(format!("/run/user/{uid}/kourier-test.lock"))
            );
        }
    }

    /// Not a test: prints the places for the lock file, for the test below to read in a child
    /// whose environment it chose.
    #[test]
    fn print_the_lock_candidates() {
        if std::env::var_os("KOURIER_LOCK_TEST_PRINT").is_none() {
            return;
        }
        println!();
        for path in lock_candidates() {
            println!("candidate {}", path.display());
        }
    }

    /// T5 review F2: two runs whose environments differ (a private `XDG_RUNTIME_DIR` of a silent
    /// run, another `HOME`, another cache) take the SAME file, or they do not exclude each other.
    #[test]
    fn two_environments_share_one_lock_file() {
        let scratch = Scratch::new("env");
        // They exist: a lock file that took the environment's runtime directory would be there.
        for dir in ["runA", "runB", "cacheA", "cacheB", "homeA", "homeB"] {
            std::fs::create_dir_all(scratch.0.join(dir)).expect("dir");
        }
        let candidates = |runtime: &str, home: &str, cache: &str| -> Vec<String> {
            let out = Command::new(std::env::current_exe().expect("this test binary"))
                .args([
                    "--exact",
                    "lock::tests::print_the_lock_candidates",
                    "--nocapture",
                    "--test-threads",
                    "1",
                ])
                .env("KOURIER_LOCK_TEST_PRINT", "1")
                .env("XDG_RUNTIME_DIR", scratch.0.join(runtime))
                .env("HOME", scratch.0.join(home))
                .env("XDG_CACHE_HOME", scratch.0.join(cache))
                .output()
                .expect("run the child");
            assert!(out.status.success(), "the child failed: {out:?}");
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .filter_map(|l| l.strip_prefix("candidate "))
                .map(str::to_owned)
                .collect()
        };
        let a = candidates("runA", "homeA", "cacheA");
        let b = candidates("runB", "homeB", "cacheB");
        assert!(!a.is_empty(), "the child printed its candidates");
        assert_eq!(a, b, "the environment moved the lock file");
        let here: Vec<String> = lock_candidates()
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        assert_eq!(a, here, "a child names the file this process names");
        assert!(
            a.iter()
                .all(|p| !p.contains(&scratch.0.display().to_string())),
            "{a:?}"
        );
    }

    /// T5 review F2: a place that cannot be opened (a file where a directory should be, a
    /// read-only bind) falls through to the next one instead of failing the whole `ci`.
    #[test]
    fn a_place_that_does_not_open_falls_through_to_the_next() {
        let scratch = Scratch::new("fall");
        let blocker = scratch.0.join("blocker");
        std::fs::write(&blocker, "a file, not a directory").expect("blocker");
        let broken = blocker.join("kourier-test.lock");
        let good = scratch.0.join("good/test.lock");
        let lock = acquire_first(
            &[broken.clone(), good.clone()],
            &who("ci"),
            false,
            &mut |_| {},
        )
        .expect("the second place is used");
        let line = std::fs::read_to_string(&good).expect("the holder line is in the second file");
        assert_eq!(
            Holder::parse(&line).expect("a holder").pid,
            std::process::id()
        );
        drop(lock);
        // Nowhere to go: one error that names the failures.
        let err = acquire_first(&[broken], &who("ci"), false, &mut |_| {}).expect_err("no place");
        assert!(
            format!("{err:#}").contains("no place for the test lock"),
            "{err:#}"
        );
        let err = acquire_first(&[], &who("ci"), false, &mut |_| {}).expect_err("no place");
        assert!(
            format!("{err:#}").contains("no place for the test lock"),
            "{err:#}"
        );
    }

    /// T5 review F8/F9: the command `cargo xtask lock -- <cmd>` starts dies with the holder. With
    /// `kill -9` of the holder the lock is free at once, and a load proof that went on would run
    /// beside the next test run.
    #[test]
    fn what_the_lock_command_starts_dies_with_the_holder() {
        let scratch = Scratch::new("pdeath");
        let lock = scratch.0.join("kourier-test.lock");
        let release = scratch.0.join("release");
        let mut holder = Child::start_with(&lock, &release, Some("die"));
        let program = holder.grandchild.expect("the child started a program");
        assert!(pid_alive(program));
        holder.process.kill().expect("kill -9 the holder");
        holder.process.wait().expect("reaped");
        let give_up = Instant::now() + Duration::from_secs(10);
        while Instant::now() < give_up && !gone(program) {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            gone(program),
            "pid {program} outlived the holder that was killed"
        );
    }

    /// Whether a process is no more (a zombie nobody has reaped yet counts as gone).
    fn gone(pid: u32) -> bool {
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Err(_) => true,
            Ok(stat) => stat
                .rsplit_once(')')
                .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z')),
        }
    }

    #[test]
    fn the_holders_line_is_read_back_and_described() {
        let holder = Holder {
            pid: 1234,
            checkout: "/home/x/kourier-wa".into(),
            command: "ci --quick".into(),
            since: 1_000_000,
        };
        assert_eq!(Holder::parse(&holder.to_json()), Some(holder.clone()));
        assert_eq!(Holder::parse(""), None);
        assert_eq!(Holder::parse("{\"pid\": 1}"), None);
        let text = holder.describe(1_000_000 + 720);
        assert!(
            text.starts_with("pid 1234 (`cargo xtask ci --quick` in /home/x/kourier-wa), since ")
        );
        assert!(text.ends_with("(12 min ago)"), "{text}");
        assert_eq!(span(0), "0 s");
        assert_eq!(span(89), "89 s");
        assert_eq!(span(90), "2 min");
        assert_eq!(span(720), "12 min");
        assert_eq!(span(3 * 3600 + 5 * 60 + 7), "3 h 5 min");
    }

    #[test]
    fn the_clock_time_is_the_local_one() {
        for unix in [0u64, 1_000_000_000, 1_792_000_000, 1_792_086_399] {
            let date = Command::new("date")
                .args(["-d", &format!("@{unix}"), "+%H:%M"])
                .output()
                .expect("date");
            assert_eq!(
                clock_time(unix),
                String::from_utf8_lossy(&date.stdout).trim(),
                "{unix}"
            );
        }
    }

    #[test]
    fn the_lock_command_line() {
        let args =
            |list: &[&str]| parse_args(&list.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>());
        assert_eq!(
            args(&["--", "cargo", "test", "--", "--list"]).unwrap(),
            (
                false,
                vec!["cargo".into(), "test".into(), "--".into(), "--list".into()]
            )
        );
        assert!(args(&["--no-wait", "--", "true"]).unwrap().0);
        assert!(args(&[]).is_err());
        assert!(args(&["--no-wait"]).is_err(), "no `--`");
        assert!(args(&["--"]).is_err(), "no command");
        assert!(args(&["true"]).is_err(), "the command follows `--`");
        assert!(args(&["--wait", "--", "true"]).is_err());
    }
}
