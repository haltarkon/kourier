//! The unit tests of `tg-run`: no network, no real keeper root (every place is below a scratch
//! directory of the test's own), nothing timed by the wall clock.

mod api;
mod connect;
mod library;
mod names;
mod parse;
mod preflight;
mod profile;
mod ranges;
mod rehearse;
mod report;
mod runbook;
mod stage;

use std::cell::Cell;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;

use super::profile::{ProfileDir, private_dir};
use super::stage::Clock;
use super::{Args, KEEPER_DIR, parse_args};

pub(super) fn strings(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| (*s).to_owned()).collect()
}

pub(super) fn args(a: &[&str]) -> Result<Args> {
    parse_args(&strings(a))
}

pub(super) fn error_of<T>(result: Result<T>) -> String {
    format!("{:#}", result.err().expect("an error"))
}

/// A scratch directory below the temporary directory (the lane's runs set `TMPDIR` to a RAM
/// directory of their own), removed when dropped. Short, so a path below it fits a socket.
pub(super) struct Scratch(pub PathBuf);

impl Scratch {
    pub fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("tgr-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        Self(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// How long a test's wait polls (the walk's `Bounds::poll`, the probe's `Wait::poll`). Long on purpose
/// (H-10): a poll that finds the child's answer ends at once, so a silent child is what costs a poll,
/// and the clock below counts exactly those.
pub(super) const POLL: Duration = Duration::from_millis(100);

/// A clock that moves only when a wait polls and finds nothing (`Clock::idle`), by the length of that
/// poll: a bound of N seconds is N seconds of SILENCE, as long as the real run's, however many times a
/// wait asked the time and however slow a child was before it said something. So a bound the test
/// passes is also the most real time a child may keep quiet (a slow `sh` under load is not elapsed
/// bound), and the most real time a test whose child never answers takes to give up. No test asserts a
/// wall time.
pub(super) struct StepClock {
    now: Cell<u64>,
    step: u64,
}

impl StepClock {
    /// A clock that moves by `poll` per idle poll.
    pub fn new(poll: Duration) -> Self {
        Self {
            now: Cell::new(0),
            step: u64::try_from(poll.as_millis()).unwrap_or(u64::MAX),
        }
    }
}

impl Clock for StepClock {
    fn now_ms(&self) -> u64 {
        self.now.get()
    }

    fn idle(&self) {
        self.now.set(self.now.get() + self.step);
    }
}

/// A profile `p` in a keeper root below the scratch directory, with its tool directory (0700).
pub(super) fn profile_in(scratch: &Scratch) -> ProfileDir {
    let dir = scratch.0.join("data").join(KEEPER_DIR).join("p");
    private_dir(&dir.join("tg-run")).expect("the profile");
    ProfileDir {
        dir,
        rehearsal: false,
    }
}

/// An executable file `name` of `text` in `dir`.
pub(super) fn executable(dir: &Path, name: &str, text: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, text).expect("a script");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("mode");
    path
}

/// The mode bits of a file.
pub(super) fn mode_of(path: &Path) -> u32 {
    std::fs::symlink_metadata(path)
        .expect("a file")
        .permissions()
        .mode()
        & 0o7777
}
