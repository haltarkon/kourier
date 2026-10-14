//! Shared by the integration tests built with `harness = false` (see `Cargo.toml`).
//!
//! cargo-nextest lists a test binary with `--list --format terse` (and again with `--ignored`)
//! and then runs one of its tests as `--exact <name> --nocapture`. A binary without libtest's
//! harness knows nothing of that: it would run its whole `main` for the listing. Each such
//! binary is one test (its `main` runs every case of the file, some in child processes of
//! their own), so [`answer_list`] gives the listing's answer, the binary's name, and
//! `main` goes on with the run for any other argument. `cargo test` passes neither flag to a
//! plain run, so nothing changes for it.

/// Answers a `--list` run with the one test this binary is, and exits. For any other
/// arguments it returns and the caller's `main` runs everything as before.
pub fn answer_list(name: &str) {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if !args.iter().any(|a| a == "--list") {
        return;
    }
    // `--ignored` asks for the ignored tests only; this binary has none.
    if !args.iter().any(|a| a == "--ignored") {
        println!("{name}: test");
    }
    std::process::exit(0);
}

/// The lines of a case's stderr that are QML script errors (`TypeError`) or failed assignments
/// ("Unable to assign [undefined] to bool"), Kirigami's included (link feedback L5): a case
/// that prints one fails, because the page-stack paths are where Kirigami evaluates a header
/// or a button of a page that is not in the stack.
#[allow(dead_code)]
pub fn script_errors(stderr: &str) -> Vec<&str> {
    stderr
        .lines()
        .filter(|l| l.contains("TypeError") || l.contains("Unable to assign"))
        .collect()
}
