//! Starting the helper process (decisions D3 H4, H5): the one file of the Telegram crates that
//! names a process command. Spawning the local helper binary contacts nothing; only the library it
//! opens could, and that happens in TG-1b's namespace tests only.
//!
//! The child gets: the socket on fd 3 (`KOURIER_TDLIB_FD=3`), stdin and stdout `/dev/null`, stderr
//! a pipe the app only counts, an EMPTY environment but that variable (and `KOURIER_DEV_NO_DUMP=1`
//! when asked), and no other descriptor: every descriptor from 4 up is marked close-on-exec before
//! the exec (`close_range` with `CLOSE_RANGE_CLOEXEC`, Linux 5.11; marking instead of closing keeps
//! the standard library's own exec-error pipe working).
//!
//! The helper is in a process group of its own (fix F7): a terminal's Ctrl-C (SIGINT) or the
//! hang-up of a closed terminal reaches the app, which then ends the helper through the one-closer
//! path (`Q` or the EOF of fd 3), not at the same instant as the app. An app whose fd 0-2 were
//! closed gets its socket pair on those numbers; the helper's end is moved above 2 first, because
//! the standard library's stdio set-up in the child would overwrite it.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, ChildStderr, Command, Stdio};

use crate::harden::NO_DUMP_ENV;
use crate::{FD_ENV, HELPER_FD};

/// What to start.
#[derive(Clone, Debug)]
pub struct SpawnArgs {
    /// The helper binary `kourier-tdlib`.
    pub helper: PathBuf,
    /// The library it opens (`--tdjson`).
    pub tdjson: PathBuf,
    /// Pass `KOURIER_DEV_NO_DUMP=1` (a debug build that holds a real account's keys).
    pub no_dump: bool,
    /// The port of the account's gate on 127.0.0.1 (`--gate-port`): the one TCP port the helper's
    /// lock-down lets it connect to, and the only port a `G` frame may name (TG-1b B-3).
    pub gate_port: u16,
    /// The account's `tdlib/` directory (`--database-dir`) and its files directory
    /// (`--files-dir`): the only places the lock-down lets TDLib write (TG-2b), and the only
    /// directories `setTdlibParameters` may name. Absolute, valid UTF-8.
    pub database_dir: PathBuf,
    pub files_dir: PathBuf,
}

/// The helper's command line: `--tdjson <path> --gate-port <1024..=65535, not a data-centre port>
/// --database-dir <absolute UTF-8 path> --files-dir <absolute UTF-8 path>`, in this order, nothing
/// else (since PROTOCOL 4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HelperCommand {
    pub tdjson: PathBuf,
    pub gate_port: u16,
    pub database_dir: String,
    pub files_dir: String,
}

const TDJSON_ARG: &str = "--tdjson";
const GATE_PORT_ARG: &str = "--gate-port";
const DATABASE_DIR_ARG: &str = "--database-dir";
const FILES_DIR_ARG: &str = "--files-dir";

/// A directory argument: valid UTF-8 (TDLib's parameters are JSON strings), absolute, no NUL.
fn dir_arg(v: std::ffi::OsString) -> Option<String> {
    v.into_string()
        .ok()
        .filter(|d| d.starts_with('/') && !d.contains('\0'))
}

/// `true` when `dir` can be a helper's directory argument; the spawner refuses any other path
/// typed BEFORE the spawn (critic L-a), so a helper's exit 2 always means "not the spawner's".
pub fn dir_arg_allowed(dir: &std::path::Path) -> bool {
    dir_arg(dir.as_os_str().to_owned()).is_some()
}

/// Reads the helper's arguments (without the program name); `None` for anything else.
pub fn parse_helper_args(
    mut args: impl Iterator<Item = std::ffi::OsString>,
) -> Option<HelperCommand> {
    let (flag, tdjson) = (args.next()?, args.next()?);
    let (port_flag, port) = (args.next()?, args.next()?);
    let (db_flag, database_dir) = (args.next()?, args.next()?);
    let (files_flag, files_dir) = (args.next()?, args.next()?);
    if flag != TDJSON_ARG
        || port_flag != GATE_PORT_ARG
        || db_flag != DATABASE_DIR_ARG
        || files_flag != FILES_DIR_ARG
        || args.next().is_some()
    {
        return None;
    }
    let gate_port = port
        .to_str()?
        .parse::<u16>()
        .ok()
        .filter(|p| crate::protocol::gate_port_allowed(*p))?;
    Some(HelperCommand {
        tdjson: PathBuf::from(tdjson),
        gate_port,
        database_dir: dir_arg(database_dir)?,
        files_dir: dir_arg(files_dir)?,
    })
}

/// A started helper.
pub struct Spawned {
    /// The app's end of the protocol socket.
    pub sock: UnixStream,
    /// The helper's stderr (counted, never shown).
    pub stderr: Option<ChildStderr>,
    pub child: Child,
}

/// `CLOSE_RANGE_CLOEXEC` of `<linux/close_range.h>`.
const CLOSE_RANGE_CLOEXEC: libc::c_uint = 1 << 2;

/// Starts the helper on a new socket pair.
pub fn spawn(args: &SpawnArgs) -> io::Result<Spawned> {
    let (app, child) = UnixStream::pair()?;
    spawn_on(args, app, child.into())
}

/// `fd` itself when it is above the standard descriptors, else a copy of it above them: the child's
/// stdio set-up (`dup2` of /dev/null onto 0, 1 and 2) runs before `pre_exec` and would overwrite it.
fn above_stdio(fd: OwnedFd) -> io::Result<OwnedFd> {
    if fd.as_raw_fd() > libc::STDERR_FILENO {
        return Ok(fd);
    }
    // SAFETY: F_DUPFD_CLOEXEC on a descriptor this function owns makes a new one, or fails with -1.
    let moved = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, HELPER_FD) };
    if moved < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `moved` is a new descriptor nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(moved) })
}

/// Starts the helper with `child_end` as its fd 3 (tests pass an end that already IS fd 3).
#[doc(hidden)]
pub fn spawn_on(args: &SpawnArgs, app: UnixStream, child_end: OwnedFd) -> io::Result<Spawned> {
    let mut cmd = Command::new(&args.helper);
    cmd.arg(TDJSON_ARG)
        .arg(&args.tdjson)
        .arg(GATE_PORT_ARG)
        .arg(args.gate_port.to_string())
        .arg(DATABASE_DIR_ARG)
        .arg(&args.database_dir)
        .arg(FILES_DIR_ARG)
        .arg(&args.files_dir);
    if args.no_dump {
        cmd.env(NO_DUMP_ENV, "1");
    }
    launch(cmd, app, child_end)
}

/// Re-executes THIS test binary as a fake helper process (layer (b) of the supervisor's tests,
/// TG-1b B-4): `--exact <test> --nocapture` with the socket on fd 3 exactly as [`spawn`] sets it
/// up, and the one variable `child` in an otherwise empty environment. It can start nothing but
/// the running executable.
#[cfg(feature = "test-support")]
#[doc(hidden)]
pub fn spawn_self(test: &str, child: (&str, &str)) -> io::Result<Spawned> {
    let (app, end) = UnixStream::pair()?;
    let mut cmd = Command::new(std::env::current_exe()?);
    cmd.args(["--exact", test, "--nocapture"]);
    cmd.env(child.0, child.1);
    launch(cmd, app, end.into())
}

/// Set in the offline child [`run_self_offline`] starts.
#[cfg(feature = "test-support")]
pub const OFFLINE_CHILD_ENV: &str = "KOURIER_TG_OFFLINE_CHILD";

/// The offline child's set-up inside `unshare -rn` (TG-1b B-5, critic H1): `lo` up and EVERY IPv4
/// and IPv6 address routed to it, so a direct connection or a DNS query that TDLib makes past the
/// gate lands on the test's trap listeners and is counted, instead of failing unseen with
/// ENETUNREACH. Then the test binary itself (`$0`) replaces the shell.
#[cfg(feature = "test-support")]
const OFFLINE_SETUP: &str = "ip link set lo up && ip route add local 0.0.0.0/0 dev lo && \
     ip -6 route add local ::/0 dev lo && exec \"$0\" \"$@\"";

/// Whether the offline child can be made here: `unshare -rn` (user namespaces) and the `ip` tool
/// for its set-up. Starts `true` only, inside an empty network namespace.
#[cfg(feature = "test-support")]
#[doc(hidden)]
pub fn offline_available() -> bool {
    Command::new("unshare")
        .args(["-rn", "sh", "-c", OFFLINE_SETUP, "true"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// What an offline child left: its exit status and everything it printed (ours only: the helper's
/// stderr is a pipe its supervisor counts).
#[cfg(feature = "test-support")]
#[doc(hidden)]
pub struct OfflineRun {
    pub status: std::process::ExitStatus,
    pub output: String,
}

/// Runs THIS test binary's one test `test` (`--exact`) in the offline child: under `unshare -rn`
/// (an empty network namespace owned by a new user namespace), with [`OFFLINE_SETUP`] done first,
/// [`OFFLINE_CHILD_ENV`] set and `env` added. Killed at `within`. It can start nothing but the
/// running executable, and the real TDLib library is only ever loaded inside such a child. Its
/// companion is [`spawn_offline_companion`]: from inside the child, and only there, the one other
/// program the lane starts (the test-only fake DC).
#[cfg(feature = "test-support")]
#[doc(hidden)]
pub fn run_self_offline(
    test: &str,
    env: &[(&str, &str)],
    within: std::time::Duration,
) -> io::Result<OfflineRun> {
    let mut cmd = Command::new("unshare");
    cmd.args(["-rn", "sh", "-c", OFFLINE_SETUP])
        .arg(std::env::current_exe()?)
        .args(["--exact", test, "--nocapture", "--test-threads=1"])
        .env(OFFLINE_CHILD_ENV, "1")
        .envs(env.iter().copied())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn()?;
    let pipes = [
        child
            .stdout
            .take()
            .map(|o| Box::new(o) as Box<dyn io::Read + Send>),
        child
            .stderr
            .take()
            .map(|e| Box::new(e) as Box<dyn io::Read + Send>),
    ];
    let readers: Vec<_> = pipes
        .into_iter()
        .flatten()
        .map(|mut pipe| {
            std::thread::spawn(move || {
                let mut text = Vec::new();
                let _ = io::Read::read_to_end(&mut pipe, &mut text);
                String::from_utf8_lossy(&text).into_owned()
            })
        })
        .collect();
    let deadline = std::time::Instant::now() + within;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            break child.wait()?;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    let output = readers
        .into_iter()
        .map(|r| r.join().unwrap_or_default())
        .collect::<Vec<_>>()
        .join("\n");
    Ok(OfflineRun { status, output })
}

/// A process [`spawn_offline_companion`] started: killed and reaped on drop. `lines` counts what
/// it printed (stdout and stderr together); the text itself is read and dropped.
#[cfg(feature = "test-support")]
#[doc(hidden)]
pub struct Companion {
    pub child: Child,
    pub lines: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

#[cfg(feature = "test-support")]
impl Drop for Companion {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Starts a test's companion process (TD-4a: the fake DC, `KOURIER_TG_FAKEDC_BIN`) IN THE OFFLINE
/// CHILD ONLY (critic L-c, lead call L8): refused (`PermissionDenied`) unless
/// [`in_offline_child`]; `program` must be an absolute path to an existing file (`InvalidInput`);
/// an EMPTY environment, stdin `/dev/null`, stdout and stderr piped and drained on threads (only
/// their lines are counted, so a chatty companion never blocks), its own process group, and
/// `PR_SET_PDEATHSIG` SIGKILL set in `pre_exec` (no race between the exec and a prctl of its own;
/// it fires when the SPAWNING THREAD ends, so call this from the test's own thread, never a tokio
/// worker). This and [`spawn`]/[`spawn_self`]/[`run_self_offline`] are the only process starts of
/// the Telegram crates.
#[cfg(feature = "test-support")]
#[doc(hidden)]
pub fn spawn_offline_companion(
    program: &std::path::Path,
    args: &[&std::ffi::OsStr],
) -> io::Result<Companion> {
    if !in_offline_child() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "a companion starts only in the offline child",
        ));
    }
    if !program.is_absolute() || !program.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the companion is not an absolute path to a file",
        ));
    }
    let mut cmd = Command::new(program);
    cmd.args(args)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let parent = std::process::id();
    // SAFETY: the closure only calls async-signal-safe functions (prctl, getppid, _exit).
    unsafe {
        cmd.pre_exec(move || {
            if libc::prctl(
                libc::PR_SET_PDEATHSIG,
                libc::SIGKILL as libc::c_ulong,
                0,
                0,
                0,
            ) < 0
            {
                return Err(io::Error::last_os_error());
            }
            // The parent may have died before the prctl: then nobody would ever kill this one.
            if libc::getppid().cast_unsigned() != parent {
                libc::_exit(1);
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn()?;
    let lines = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let pipes = [
        child
            .stdout
            .take()
            .map(|o| Box::new(o) as Box<dyn io::Read + Send>),
        child
            .stderr
            .take()
            .map(|e| Box::new(e) as Box<dyn io::Read + Send>),
    ];
    for pipe in pipes.into_iter().flatten() {
        let seen = lines.clone();
        std::thread::spawn(move || {
            use std::io::BufRead as _;
            let mut reader = io::BufReader::new(pipe);
            let mut line = Vec::new();
            while reader.read_until(b'\n', &mut line).is_ok_and(|n| n > 0) {
                seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                line.clear();
            }
        });
    }
    Ok(Companion { child, lines })
}

/// True only inside the offline child: [`OFFLINE_CHILD_ENV`] is set AND this process's network
/// namespace holds no interface but `lo` (read from `/proc/self/net/dev`, which follows the
/// process's namespace; `/sys/class/net` follows sysfs's mount). The real-library tests check it
/// before they load the library.
#[cfg(feature = "test-support")]
#[doc(hidden)]
pub fn in_offline_child() -> bool {
    std::env::var_os(OFFLINE_CHILD_ENV).is_some() && offline_namespace()
}

/// Whether this process's network namespace holds no interface but `lo` (a helper the offline child
/// started is in it too, with an emptied environment).
#[cfg(feature = "test-support")]
#[doc(hidden)]
pub fn offline_namespace() -> bool {
    let Ok(dev) = std::fs::read_to_string("/proc/self/net/dev") else {
        return false;
    };
    let names: Vec<&str> = dev
        .lines()
        .skip(2)
        .filter_map(|l| l.split(':').next())
        .map(str::trim)
        .collect();
    names == ["lo"]
}

/// The protocol socket of a process the spawner started (fd 3, named by `KOURIER_TDLIB_FD`), marked
/// close-on-exec; `None` in any other process. Call it once: the socket owns the descriptor.
#[cfg(feature = "test-support")]
#[doc(hidden)]
pub fn helper_socket() -> Option<UnixStream> {
    if std::env::var_os(FD_ENV).is_none_or(|v| v != HELPER_FD.to_string().as_str()) {
        return None;
    }
    // SAFETY: the spawner put the socket on fd 3 for this process; `fstat` says whether it is one.
    let is_socket = unsafe {
        let mut st: libc::stat = std::mem::zeroed();
        libc::fstat(HELPER_FD, &mut st) == 0 && (st.st_mode & libc::S_IFMT) == libc::S_IFSOCK
    };
    if !is_socket {
        return None;
    }
    // SAFETY: fd 3 is a socket of this process that nothing else owns (see above).
    unsafe {
        libc::fcntl(HELPER_FD, libc::F_SETFD, libc::FD_CLOEXEC);
        Some(UnixStream::from_raw_fd(HELPER_FD))
    }
}

/// Starts `cmd` with `child_end` as its fd 3: empty environment but the protocol variable (and
/// what `cmd` already set), stdin and stdout `/dev/null`, stderr piped, its own process group, every
/// other descriptor close-on-exec.
fn launch(mut cmd: Command, app: UnixStream, child_end: OwnedFd) -> io::Result<Spawned> {
    let child_end = above_stdio(child_end)?;
    let fd = child_end.as_raw_fd();
    let kept: Vec<_> = cmd
        .get_envs()
        .filter_map(|(k, v)| Some((k.to_owned(), v?.to_owned())))
        .collect();
    cmd.env_clear()
        .envs(kept)
        .env(FD_ENV, HELPER_FD.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        // Its own process group: no terminal signal reaches it at the app's instant (F7).
        .process_group(0);
    // SAFETY: the closure only calls async-signal-safe functions (fcntl, dup2, close_range).
    unsafe {
        cmd.pre_exec(move || {
            if fd == HELPER_FD {
                // dup2 onto itself would keep FD_CLOEXEC: clear it.
                let flags = libc::fcntl(fd, libc::F_GETFD);
                if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                    return Err(io::Error::last_os_error());
                }
            } else if libc::dup2(fd, HELPER_FD) < 0 {
                return Err(io::Error::last_os_error());
            }
            let first = (HELPER_FD + 1) as libc::c_uint;
            if libc::syscall(
                libc::SYS_close_range,
                first,
                libc::c_uint::MAX,
                CLOSE_RANGE_CLOEXEC,
            ) < 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn()?;
    drop(child_end);
    let stderr = child.stderr.take();
    Ok(Spawned {
        sock: app,
        stderr,
        child,
    })
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStrExt as _;

    use super::*;

    fn parse(args: &[&str]) -> Option<HelperCommand> {
        parse_helper_args(args.iter().map(OsString::from))
    }

    /// The spawner's command line reads back; a missing, zero, oversized or extra part is refused.
    #[test]
    fn the_command_line_reads_back_and_nothing_else_does() {
        const D: [&str; 4] = ["--database-dir", "/a/tdlib", "--files-dir", "/f/0123"];
        let full = |head: &[&'static str]| -> Vec<&'static str> {
            head.iter().copied().chain(D).collect()
        };
        assert_eq!(
            parse(&full(&["--tdjson", "/x/lib.so", "--gate-port", "40001"])),
            Some(HelperCommand {
                tdjson: PathBuf::from("/x/lib.so"),
                gate_port: 40001,
                database_dir: "/a/tdlib".to_owned(),
                files_dir: "/f/0123".to_owned(),
            })
        );
        // The directories: both, in this order, absolute.
        for bad in [
            &["--tdjson", "/x/lib.so", "--gate-port", "40001"][..],
            &[
                "--tdjson",
                "/x/lib.so",
                "--gate-port",
                "40001",
                "--database-dir",
                "/a/tdlib",
            ],
            &[
                "--tdjson",
                "/x/lib.so",
                "--gate-port",
                "40001",
                "--files-dir",
                "/f/0123",
                "--database-dir",
                "/a/tdlib",
            ],
            &[
                "--tdjson",
                "/x/lib.so",
                "--gate-port",
                "40001",
                "--database-dir",
                "a/tdlib",
                "--files-dir",
                "/f/0123",
            ],
            &[
                "--tdjson",
                "/x/lib.so",
                "--gate-port",
                "40001",
                "--database-dir",
                "/a/tdlib",
                "--files-dir",
                "",
            ],
            &[
                "--tdjson",
                "/x/lib.so",
                "--gate-port",
                "40001",
                "--database-dir",
                "/a/tdlib",
                "--files-dir",
                "/f/0123",
                "--more",
            ],
        ] {
            assert_eq!(parse(bad), None, "{bad:?} was taken");
        }
        // Not UTF-8: refused here, and by the spawner before any spawn.
        let not_utf8 = OsString::from(std::ffi::OsStr::from_bytes(b"/a/\xff"));
        let args = [
            "--tdjson",
            "/x/lib.so",
            "--gate-port",
            "40001",
            "--database-dir",
        ]
        .iter()
        .map(OsString::from)
        .chain([not_utf8.clone(), "--files-dir".into(), "/f/0123".into()]);
        assert_eq!(parse_helper_args(args), None);
        assert!(!dir_arg_allowed(std::path::Path::new(&not_utf8)));
        assert!(!dir_arg_allowed(std::path::Path::new("rel/tdlib")));
        assert!(dir_arg_allowed(std::path::Path::new("/a/tdlib")));
        for bad in [
            &["--tdjson", "/x/lib.so"][..],
            &["--tdjson", "/x/lib.so", "--gate-port", "0"],
            &["--tdjson", "/x/lib.so", "--gate-port", "65536"],
            // G6: a privileged port, and each data-centre port, is no gate's port.
            &["--tdjson", "/x/lib.so", "--gate-port", "1023"],
            &["--tdjson", "/x/lib.so", "--gate-port", "443"],
            &["--tdjson", "/x/lib.so", "--gate-port", "80"],
            &["--tdjson", "/x/lib.so", "--gate-port", "5222"],
            &["--tdjson", "/x/lib.so", "--gate-port", "-1"],
            &["--tdjson", "/x/lib.so", "--gate-port", "40001", "--more"],
            &["--gate-port", "1", "--tdjson", "/x/lib.so"],
            &[],
        ] {
            assert_eq!(parse(&full(bad)), None, "{bad:?} was taken");
        }
    }
}
