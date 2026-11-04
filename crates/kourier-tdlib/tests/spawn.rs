//! Starting the REAL helper binary (decisions D3 H4, H5) with a MISSING library: no TDLib is
//! loaded, so nothing can contact anything. What the child gets: fd 3 (also when the app's end
//! already is fd 3), stdin and stdout `/dev/null`, an empty environment but `KOURIER_TDLIB_FD=3`,
//! no inherited descriptor, exit 3 after `F lib-open-failed`.

use std::collections::BTreeSet;
use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use kourier_tdlib::protocol::{FailureCode, FromHelper, LineReader, decode_from_helper};
use kourier_tdlib::spawn::{SpawnArgs, Spawned, spawn, spawn_on};

const CHILD_ENV: &str = "KOURIER_TDLIB_TEST_CHILD";

fn args(no_dump: bool) -> SpawnArgs {
    SpawnArgs {
        helper: PathBuf::from(env!("CARGO_BIN_EXE_kourier-tdlib")),
        tdjson: PathBuf::from("/nonexistent/kourier-test/libtdjson.so"),
        no_dump,
        gate_port: 40001,
        database_dir: PathBuf::from("/nonexistent/kourier-test/tdlib"),
        files_dir: PathBuf::from("/nonexistent/kourier-test/files"),
    }
}

/// The first frame the helper writes.
fn first_frame(sock: &UnixStream) -> FromHelper {
    sock.set_read_timeout(Some(Duration::from_secs(10)))
        .expect("timeout");
    let mut lines = LineReader::new(sock.try_clone().expect("clone"));
    let line = match lines.next_line() {
        Ok(Some(l)) => l,
        _ => panic!("the helper wrote no frame on fd 3"),
    };
    decode_from_helper(&line).expect("the helper's line is a frame")
}

fn assert_open_failed(s: &Spawned) {
    match first_frame(&s.sock) {
        FromHelper::Failure {
            code: FailureCode::LibOpenFailed,
            extra: None,
        } => {}
        other => panic!("expected F lib-open-failed, got {other:?}"),
    }
}

/// Waits for the exit and reads the stderr byte count (never its text).
fn finish(mut s: Spawned) -> (Option<i32>, usize) {
    drop(s.sock);
    let mut err = Vec::new();
    if let Some(mut e) = s.stderr.take() {
        let _ = e.read_to_end(&mut err);
    }
    let status = s.child.wait().expect("wait");
    (status.code(), err.len())
}

#[test]
fn a_missing_library_is_reported_on_fd_3_and_the_child_gets_nothing_else() {
    // A descriptor the test process leaks on purpose (no close-on-exec, above 3 so the spawner's
    // dup2 onto fd 3 cannot hide it): the child must not get it.
    let file = std::fs::File::open("/dev/null").expect("open");
    // SAFETY: F_DUPFD makes a new descriptor of this test (without close-on-exec), owned below.
    let leaked = unsafe {
        let fd = libc::fcntl(file.as_raw_fd(), libc::F_DUPFD, 20);
        assert!(fd >= 20, "no descriptor to leak");
        OwnedFd::from_raw_fd(fd)
    };
    drop(file);
    let s = spawn(&args(false)).expect("spawn the helper");
    drop(leaked);
    assert_open_failed(&s);
    let proc = PathBuf::from(format!("/proc/{}", s.child.id()));
    let environ = std::fs::read(proc.join("environ")).expect("read the child's environ");
    assert_eq!(
        environ, b"KOURIER_TDLIB_FD=3\0",
        "the child's environment is not just KOURIER_TDLIB_FD=3"
    );
    let cmdline = std::fs::read(proc.join("cmdline")).expect("read the child's command line");
    let tail: Vec<&[u8]> = cmdline.split(|b| *b == 0).skip(1).collect();
    assert_eq!(
        tail,
        [
            &b"--tdjson"[..],
            b"/nonexistent/kourier-test/libtdjson.so",
            b"--gate-port",
            b"40001",
            b"--database-dir",
            b"/nonexistent/kourier-test/tdlib",
            b"--files-dir",
            b"/nonexistent/kourier-test/files",
            b""
        ],
        "the child's arguments are not the library, the gate's port and the two directories"
    );
    let fds: BTreeSet<String> = std::fs::read_dir(proc.join("fd"))
        .expect("read the child's fds")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    let want: BTreeSet<String> = ["0", "1", "2", "3"].map(String::from).into();
    assert_eq!(fds, want, "the child holds other descriptors than 0-3");
    for n in ["0", "1"] {
        let target = std::fs::read_link(proc.join("fd").join(n)).expect("readlink");
        assert_eq!(
            target,
            PathBuf::from("/dev/null"),
            "stdin or stdout is not /dev/null"
        );
    }
    let (code, err_bytes) = finish(s);
    assert_eq!(code, Some(3), "a missing library is exit 3");
    assert_eq!(err_bytes, 0, "the helper wrote to stderr");
}

/// The child half of the equal-fd case: the app's child end IS fd 3 before the spawn (dup2 onto
/// itself would keep close-on-exec, and the helper would start without its socket).
#[test]
fn equal_fd_child() {
    if std::env::var_os(CHILD_ENV).is_none() {
        return;
    }
    // Keeps fd 3 taken while the pair is made, so neither end of it is fd 3 yet; it is replaced
    // by the dup2 below (and so never closed as a file).
    std::mem::forget(std::fs::File::open("/dev/null").expect("open"));
    let (app, child) = UnixStream::pair().expect("pair");
    assert!(app.as_raw_fd() != 3 && child.as_raw_fd() != 3);
    // SAFETY: this process is the test's own child; fd 3 is overwritten on purpose and then
    // marked close-on-exec like every end of a socket pair.
    let three = unsafe {
        assert!(libc::dup2(child.as_raw_fd(), 3) == 3);
        libc::fcntl(3, libc::F_SETFD, libc::FD_CLOEXEC);
        OwnedFd::from_raw_fd(3)
    };
    drop(child);
    let s = spawn_on(&args(false), app, three).expect("spawn the helper");
    assert_open_failed(&s);
    let (code, _) = finish(s);
    assert_eq!(code, Some(3));
}

#[test]
fn an_end_that_already_is_fd_3_still_reaches_the_helper() {
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "spawn::equal_fd_child", "--nocapture"])
        .env(CHILD_ENV, "1")
        .output()
        .expect("start the child");
    assert!(
        out.status.success()
            && String::from_utf8_lossy(&out.stdout).contains("test result: ok. 1 passed"),
        "the helper did not get its socket when the app's end was fd 3"
    );
}

/// D3 H5: `KOURIER_DEV_NO_DUMP=1` reaches the helper through the cleared environment and makes it
/// non-dumpable: its environment can no longer be read by the same user, and its core limit is
/// {1, 1} (K1: 0 would leave a pipe `core_pattern` such as systemd-coredump free to store its
/// memory; 1 is refused by the kernel for a pipe and is under a page for a file).
#[test]
fn no_dump_is_passed_through_and_the_helper_denies_dumps() {
    let s = spawn(&args(true)).expect("spawn the helper");
    assert_open_failed(&s);
    let proc = PathBuf::from(format!("/proc/{}", s.child.id()));
    let environ = std::fs::read(proc.join("environ"));
    assert!(
        environ.is_err(),
        "a non-dumpable helper's environment is readable"
    );
    let limits = std::fs::read_to_string(proc.join("limits")).expect("read the limits");
    let core: Vec<&str> = limits
        .lines()
        .find(|l| l.starts_with("Max core file size"))
        .expect("a core line")
        .split_whitespace()
        .collect();
    assert_eq!(
        core[4..6],
        ["1", "1"],
        "the helper's core limit is not {{1, 1}}"
    );
    let (code, _) = finish(s);
    assert_eq!(code, Some(3));
}

/// F7: the helper is the leader of a process group of its own, so a terminal's Ctrl-C or SIGHUP
/// aimed at the app's group does not reach it; the app ends it through fd 3.
#[test]
fn the_helper_leads_a_process_group_of_its_own() {
    let s = spawn(&args(false)).expect("spawn the helper");
    assert_open_failed(&s);
    let pid = s.child.id() as i32;
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).expect("read the stat");
    // After the command name in parentheses: state, ppid, pgrp, session.
    let rest = stat.rsplit_once(") ").expect("a stat line").1;
    let pgrp: i32 = rest
        .split_whitespace()
        .nth(2)
        .and_then(|f| f.parse().ok())
        .expect("a process group");
    // SAFETY: getpgrp has no arguments and cannot fail.
    let ours = unsafe { libc::getpgrp() };
    assert_eq!(pgrp, pid, "the helper is not the leader of its own group");
    assert_ne!(pgrp, ours, "the helper shares the app's process group");
    let (code, _) = finish(s);
    assert_eq!(code, Some(3));
}

/// The child half of the closed-stdio case: the app's descriptors 0-2 are closed, so the socket
/// pair of `spawn` lands on them. The exit code reports (libtest's own output would go to the
/// socket).
#[test]
fn closed_stdio_child() {
    if std::env::var_os(CHILD_ENV).is_none() {
        return;
    }
    // SAFETY: this process is the test's own child; it gives up its stdio on purpose.
    unsafe {
        libc::close(0);
        libc::close(1);
        libc::close(2);
    }
    let s = spawn(&args(false)).expect("spawn the helper with closed stdio");
    assert_open_failed(&s);
    let (code, _) = finish(s);
    std::process::exit(if code == Some(3) { 42 } else { 43 });
}

#[test]
fn an_app_with_closed_stdio_still_gets_its_helper_going() {
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "spawn::closed_stdio_child", "--nocapture"])
        .env(CHILD_ENV, "1")
        .output()
        .expect("start the child");
    assert_eq!(
        out.status.code(),
        Some(42),
        "the helper did not get its socket when the app's descriptors 0-2 were closed"
    );
}

/// TD-4a C3 (critic L-c): the fake DC's starter refuses outside the offline child, before it looks
/// at the program (an existing absolute file here), and starts nothing.
#[test]
fn a_companion_is_refused_outside_the_offline_child() {
    assert!(
        !kourier_tdlib::spawn::in_offline_child(),
        "this test runs outside the offline child"
    );
    for program in ["/bin/true", "true"] {
        let err = kourier_tdlib::spawn::spawn_offline_companion(Path::new(program), &[])
            .err()
            .expect("a companion started outside the offline child");
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
    }
}
