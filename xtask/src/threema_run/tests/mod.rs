//! The unit tests of `threema-run`: no network, no real keeper root (every place is below a scratch
//! directory of the test's own), nothing timed by the wall clock, and no value that looks like a real
//! Threema ID or a digest of one (the forbidden values are built from parts).

mod backup;
mod fix;
mod judge;
mod launch;
mod lines;
mod names;
mod parse;
mod preflight;
mod profile;
mod prompt;
mod rehearse;
mod report;
mod runbook;
mod steps;
mod supervise;
mod tphone;

use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;

use anyhow::Result;

use super::profile::private_dir;
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

/// A value with the shape of a Threema ID that is nothing of anyone's, built from parts.
pub(super) fn id_shape() -> String {
    ["SYN", "TH", "T01"].concat()
}

/// A value with the shape of a SHA-256 digest that is nothing of anyone's, built from parts.
pub(super) fn digest_shape() -> String {
    "9f".repeat(32)
}

/// A scratch directory below the temporary directory (the lane's runs set `TMPDIR` to a RAM
/// directory of their own), canonical (the tool refuses a place with a link on its way) and removed
/// when dropped. Short, so a path below it stays short.
pub(super) struct Scratch(pub PathBuf);

impl Scratch {
    pub fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("thr-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        Self(std::fs::canonicalize(&dir).expect("canonical scratch"))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A data home with its keeper root (0700) below a scratch directory, and a `target` beside it.
pub(super) struct World {
    pub scratch: Scratch,
}

impl World {
    pub fn new(name: &str) -> Self {
        let w = Self {
            scratch: Scratch::new(name),
        };
        private_dir(&w.keeper()).expect("the keeper root");
        w
    }

    pub fn data(&self) -> PathBuf {
        self.scratch.0.join("data")
    }

    pub fn keeper(&self) -> PathBuf {
        self.data().join(KEEPER_DIR)
    }

    pub fn target(&self) -> PathBuf {
        self.scratch.0.join("target")
    }

    /// The directory of the profile `p` (not made).
    pub fn profile(&self) -> PathBuf {
        self.keeper().join("p")
    }

    /// The list of test IDs in the keeper root (not made).
    pub fn allow_file(&self) -> PathBuf {
        self.keeper().join(super::ALLOW_FILE)
    }
}

/// The mode bits of a file.
pub(super) fn mode_of(path: &std::path::Path) -> u32 {
    std::fs::symlink_metadata(path)
        .expect("a file")
        .permissions()
        .mode()
        & 0o7777
}
