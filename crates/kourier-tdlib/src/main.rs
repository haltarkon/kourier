//! The helper binary `kourier-tdlib` (TG-1 plan §5.1; decisions D3 H4, H5, M3).
//!
//! `kourier-tdlib --tdjson <path> --gate-port <p> --database-dir <p> --files-dir <p>` with
//! `KOURIER_TDLIB_FD=3`: hardens the process (core limit {1, 1} in every build, which refuses a
//! dump to a file AND to systemd-coredump's pipe; release builds also non-dumpable) BEFORE it
//! opens the library, opens it, LOCKS IT DOWN (`lockdown::apply`: no UDP,
//! TCP only to the gate's port, TG-1b B-3; files only in the two account directories and a few
//! system files read only, TG-2b) before the serve loop makes
//! any client, serves the app on fd 3 (a `G` frame for another port is a bad frame), and leaves with
//! `_exit` (no static destructors run while TDLib's threads live). The lock-down has no switch.
//!
//! | Exit | Meaning |
//! |---|---|
//! | 0 | every client closed |
//! | 2 | started without fd 3 or without the spawner's arguments (not by the spawner) |
//! | 3 | the library could not be opened or lacks an entry point (`F lib-open-failed`/`lib-symbol-missing` first) |
//! | 4 | the close deadline passed (`B {"forced":true}` first) |
//! | 5 | a fatal frame ended the serve loop |
//! | 6 | the lock-down failed (`F lock-down-failed` first; no client was made) |
//! | 7 | TDLib's fatal path (CHECK, LOG(FATAL)): `F tdlib-fatal` first, then `_exit` instead of TDLib's `abort` (pre3 P-3; any OTHER crash is a signal, with no core dump: the core limit is {1, 1} in every build, K1) |

use std::os::fd::FromRawFd;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use kourier_tdlib::dlopen::{self, LibError};
use kourier_tdlib::protocol::FailureCode;
use kourier_tdlib::serve::{ServeEnd, ServeOptions, serve};
use kourier_tdlib::spawn::parse_helper_args;
use kourier_tdlib::{FD_ENV, HELPER_FD, harden, lockdown, refuse};

fn leave(code: i32) -> ! {
    // SAFETY: `_exit` ends the process at once; nothing of ours waits for a destructor.
    unsafe { libc::_exit(code) }
}

fn main() {
    harden::deny_dumps();
    if std::env::var_os(FD_ENV).is_none_or(|v| v != HELPER_FD.to_string().as_str()) {
        leave(2);
    }
    // SAFETY: the spawner put the socket on fd 3 for this process and nothing else owns it.
    let sock = unsafe { UnixStream::from_raw_fd(HELPER_FD) };
    // TDLib starts no programs, but none would get the socket either.
    // SAFETY: fcntl on a descriptor of this process.
    unsafe {
        libc::fcntl(HELPER_FD, libc::F_SETFD, libc::FD_CLOEXEC);
    }
    let Some(command) = parse_helper_args(std::env::args_os().skip(1)) else {
        leave(2);
    };
    let lib = match dlopen::open(&command.tdjson) {
        Ok(lib) => lib,
        Err(LibError::Symbol(_)) => refuse_and_leave(sock, FailureCode::LibSymbolMissing, 3),
        Err(LibError::Open) => refuse_and_leave(sock, FailureCode::LibOpenFailed, 3),
    };
    // After the open (the library's file is read), before any client (TDLib's threads start
    // with the first one, and every thread inherits the lock-down).
    let dirs = lockdown::FsPaths {
        database_dir: Path::new(&command.database_dir),
        files_dir: Path::new(&command.files_dir),
    };
    let landlock = match lockdown::apply(command.gate_port, &dirs) {
        Ok(state) => state,
        Err(_) => refuse_and_leave(sock, FailureCode::LockDownFailed, 6),
    };
    let opts = ServeOptions::helper(
        landlock.as_str(),
        command.gate_port,
        command.database_dir.clone(),
        command.files_dir.clone(),
    );
    let end = serve(lib, sock, opts);
    leave(match end {
        ServeEnd::AllClosed => 0,
        ServeEnd::Forced => 4,
        ServeEnd::Fatal(_) => 5,
    });
}

/// Writes the code, waits until the app closes its end (at most 5 s in all, so the app can look at
/// the process first), and leaves with `exit`.
fn refuse_and_leave(sock: UnixStream, code: FailureCode, exit: i32) -> ! {
    refuse::announce_and_wait(sock, code, Duration::from_secs(5));
    leave(exit)
}
