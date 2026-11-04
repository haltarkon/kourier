//! Tests that spawn an actor run it in an OFFLINE CHILD (W2, lane decision L3): the test binary
//! started again under `unshare -rn`, an empty network namespace with no interface and no route, so
//! that whatever the actor does with its parts it cannot reach a network, on top of the guards that
//! refuse every dial and request (`support::guards`). Where user namespaces are unavailable the test
//! is skipped with a message and never falls back to the real network - unless `KOURIER_REQUIRE_UNSHARE=1`
//! (which `cargo xtask ci` sets) says a skip is a failure: the Wire cases that carry the receive path's
//! proof would otherwise pass with nothing run (W3a fix F12).
//!
//! The parent test calls [`run_child`]; the child is a test of the same module that begins with
//! `if !support::offline::in_child() { return; }` and does the work.

use std::process::Command;

/// Set in the child.
const CHILD: &str = "WA_OFFLINE_CHILD";

/// When this is `1`, a test that needs `unshare -rn` FAILS where it is unavailable instead of
/// skipping. Set by `cargo xtask ci` (`xtask/src/runner.rs`).
pub const REQUIRE_ENV: &str = "KOURIER_REQUIRE_UNSHARE";

/// What a test that needs `unshare -rn` does here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    /// It works: run.
    Run,
    /// It does not, and nobody insisted: skip with a message.
    Skip,
    /// It does not, and [`REQUIRE_ENV`] is `1`: fail.
    Fail,
}

/// The decision, from the two facts (`required` is the variable's value).
pub fn gate(available: bool, required: Option<&str>) -> Gate {
    match (available, required) {
        (true, _) => Gate::Run,
        (false, Some("1")) => Gate::Fail,
        (false, _) => Gate::Skip,
    }
}

/// `true` when the test may run its offline child. Where `unshare -rn` is unavailable it prints
/// `SKIPPED: <why>` and returns `false`, or PANICS when [`REQUIRE_ENV`] is `1` (a green run must
/// have run the test).
pub fn unshare_or_skip(why: &str) -> bool {
    match gate(
        unshare_available(),
        std::env::var(REQUIRE_ENV).ok().as_deref(),
    ) {
        Gate::Run => true,
        Gate::Skip => {
            eprintln!("SKIPPED: `unshare -rn` is not available (user namespaces), and {why}");
            false
        }
        Gate::Fail => panic!(
            "`unshare -rn` is not available and {REQUIRE_ENV}=1: this test must run, not skip              ({why})"
        ),
    }
}

/// Whether `unshare -rn` works here (user namespaces can be disabled).
pub fn unshare_available() -> bool {
    Command::new("unshare")
        .args(["-rn", "true"])
        .status()
        .is_ok_and(|status| status.success())
}

/// True inside the child started by [`run_child`].
pub fn in_child() -> bool {
    std::env::var_os(CHILD).is_some()
}

/// The command that starts this binary under `unshare -rn` for the one test `child` of `module`
/// (`module_path!()` of the calling file), for a parent that wants to read the child's output while
/// it runs or to kill it. Only after [`unshare_available`] said yes.
pub fn child_command(module: &str, child: &str) -> Command {
    let exe = std::env::current_exe().expect("test binary");
    let mut command = Command::new("unshare");
    command
        .args(["-rn"])
        .arg(exe)
        .arg("--exact")
        .arg(super::child_filter(module, child))
        .args(["--nocapture", "--test-threads=1"])
        .env(CHILD, "1");
    command
}

/// The parent half: starts this binary under `unshare -rn` for the one test `child` of `module`
/// (`module_path!()` of the calling file) and fails unless exactly that test passed.
pub fn run_child(module: &str, child: &str) {
    let _ = run_child_output(module, child, &[]);
}

/// As [`run_child`], with `env` set in the child (a test that runs one case of several chosen by
/// the parent), and returns everything the child wrote to stdout and stderr (empty when the test
/// was skipped), for a parent that looks in it for what must never be printed.
pub fn run_child_output(module: &str, child: &str, env: &[(&str, &str)]) -> String {
    if !unshare_or_skip("this test never runs an actor on the real network") {
        return String::new();
    }
    let mut command = child_command(module, child);
    command.envs(env.iter().copied());
    let out = command.output().expect("spawn the child under unshare");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success() && stdout.contains("test result: ok. 1 passed;"),
        "the child failed ({:?}):\n{stdout}\n{stderr}",
        out.status,
    );
    format!("{stdout}\n{stderr}")
}
