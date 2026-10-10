//! What protects a profile that holds a real account when the debug build runs on it
//! (`cargo xtask smoke`), with the real binary:
//!
//! - `KOURIER_DEV_NO_DUMP=1` makes the debug build non-dumpable like a release build: its
//!   memory cannot be read through `/proc`, and a process that is made to abort leaves no
//!   core dump (architecture §9.4). Without the variable a debug build stays dumpable.
//! - One process per profile: a second start on the same profile is refused by the lock on
//!   its data directory, also where no bus name could say so (offscreen, desktop services
//!   off), and it changes nothing.
//!
//! Debug builds only (the switch and the development keyring exist only there). Nothing here
//! links an account or opens a connection: the profiles are empty.

#![cfg(debug_assertions)]

use std::io::{BufRead as _, BufReader};
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const WAIT: Duration = Duration::from_secs(90);

fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// The binary offscreen, without desktop services, on `home` with the development keyring.
fn kourier(dir: &Path, home: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_kourier"));
    cmd.arg("--dev-control")
        .env("KOURIER_HOME", home)
        .env("KOURIER_DEV_KEYRING", "passphrase")
        .env("KOURIER_DESKTOP", "0")
        .env_remove("KOURIER_SIGNAL_ENV")
        .env_remove("KOURIER_FAILPOINT")
        .env_remove("KOURIER_DEV_NO_DUMP")
        .env("QT_QPA_PLATFORM", "offscreen")
        .env("QT_QUICK_BACKEND", "software")
        .env("QT_SCALE_FACTOR", "1")
        .env("QT_FORCE_STDERR_LOGGING", "1")
        .env("TMPDIR", dir)
        .env("LANG", "en_US.UTF-8")
        .env("LANGUAGE", "en")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped());
    cmd
}

/// Starts `cmd` and waits until its development channel is up (keys are loaded by then).
fn started(mut cmd: Command, log: &Path) -> Child {
    let mut child = cmd
        .stderr(Stdio::from(std::fs::File::create(log).expect("log")))
        .spawn()
        .expect("start kourier");
    let stdout = child.stdout.take().expect("stdout");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if line.contains("\"channel\"") {
                let _ = tx.send(());
            }
        }
    });
    rx.recv_timeout(WAIT)
        .unwrap_or_else(|_| panic!("the channel did not come up (log: {})", log.display()));
    child
}

fn wait_exit(child: &mut Child) -> std::process::ExitStatus {
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            return status;
        }
        assert!(Instant::now() < deadline, "kourier did not exit");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Whether this user may read the process's memory map summary: only of a dumpable process.
fn memory_readable(pid: u32) -> bool {
    std::fs::read(format!("/proc/{pid}/smaps_rollup")).is_ok()
}

#[test]
fn a_debug_build_asked_not_to_dump_hides_its_memory_and_leaves_no_core() {
    let dir = scratch("hardening-dump");

    // The control: a debug build is dumpable by itself. (It is quit, not aborted: an abort
    // of a dumpable process is exactly the core dump this test is about.)
    let mut plain = started(kourier(&dir, &dir.join("plain")), &dir.join("plain.log"));
    assert!(
        memory_readable(plain.id()),
        "a debug build without the switch stays dumpable"
    );
    drop(plain.stdin.take());
    assert!(wait_exit(&mut plain).success());

    let mut cmd = kourier(&dir, &dir.join("guarded"));
    cmd.env("KOURIER_DEV_NO_DUMP", "1");
    let mut guarded = started(cmd, &dir.join("guarded.log"));
    let pid = guarded.id();
    assert!(
        !memory_readable(pid),
        "with the switch the process is not dumpable"
    );
    // Made to abort, as a failed debug assertion or a failpoint would: the kernel writes no
    // core for a process whose dumpable flag is off, whatever `core_pattern` pipes to.
    let killed = Command::new("kill")
        .args(["-ABRT", &pid.to_string()])
        .status()
        .expect("kill");
    assert!(killed.success());
    let status = wait_exit(&mut guarded);
    assert_eq!(status.signal(), Some(6), "ended by SIGABRT: {status:?}");
    assert!(
        !status.core_dumped(),
        "no core dump was written: {status:?}"
    );
}

#[test]
fn a_second_process_on_a_profile_is_refused_and_changes_nothing() {
    let dir = scratch("hardening-lock");
    let home = dir.join("home");
    let mut first = started(kourier(&dir, &home), &dir.join("first.log"));
    let keyring = home.join("kourier/config/dev-keyring.json");
    let before = std::fs::read(&keyring).expect("the first start made the keyring");

    // No session bus name exists here (desktop services off): only the lock can say no.
    let second = kourier(&dir, &home)
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .expect("second start");
    assert_eq!(second.status.code(), Some(1), "{second:?}");
    let said = String::from_utf8_lossy(&second.stderr);
    assert!(
        said.contains("another Kourier process is using this profile"),
        "{said}"
    );
    assert!(
        first.try_wait().expect("try_wait").is_none(),
        "the first runs on"
    );
    assert_eq!(std::fs::read(&keyring).expect("keyring"), before);

    drop(first.stdin.take());
    assert!(wait_exit(&mut first).success());
    // The lock went with its holder: the profile starts again.
    let mut again = started(kourier(&dir, &home), &dir.join("again.log"));
    drop(again.stdin.take());
    assert!(wait_exit(&mut again).success());
}
