//! Helpers shared by the integration tests of `kourier-whatsapp`.

// Every test module uses only part of this.
#![allow(dead_code, unused_macros, unused_imports)]

use std::path::{Path, PathBuf};

use kourier_db::DbKey;
use kourier_whatsapp::{KourierStore, StoreOwner};

// The CDN fake of the media tests (W5b): the clients' HTTP client that answers upstream's uploads.
pub mod cdn;
// The recording `tracing` subscriber of the tests that read what Kourier logs (W3a, D10).
pub mod capture;
// The refusing, counting transport factory and HTTP client of every test that builds a client.
pub mod guards;
// Hand-made `HistorySync` chunks (W4): the library's own builder source, included by path.
pub mod history;
// The kill tests' log rule (one write per line, a torn tail cut before appending, complete lines only): the library's
// own `src/kill_log.rs`, included by path, for the core's notes of `send_kill` and the Wire's and the Cdn fake's logs.
#[path = "../../src/kill_log.rs"]
pub mod kill_log;
// The `unshare -rn` child of every test that runs an actor.
pub mod offline;
// The fictitious contact that encrypts messages to the test account (W3a).
pub mod peer;

// whatsapp-rust's own store contract helpers, copied verbatim (MIT, re-copy on every pin bump).
#[path = "upstream/appstate_commit_contract.rs"]
pub mod appstate_commit_contract;
#[path = "upstream/tc_token_contract.rs"]
pub mod tc_token_contract;
// The scripted server of the offline wire tests: own code after upstream's in-process responder (MIT).
#[path = "upstream/wire.rs"]
pub mod wire;

/// The key of every test store. A fixed TEST value: it protects nothing.
pub const TEST_KEY: [u8; 32] = [0x57; 32];

/// The key as the database layer takes it.
pub fn key() -> DbKey {
    DbKey::from(TEST_KEY)
}

/// A store on a fresh file in a temp directory. The directory goes with the value.
pub struct TempStore {
    pub dir: tempfile::TempDir,
    pub owner: StoreOwner,
}

impl TempStore {
    pub fn store(&self) -> KourierStore {
        self.owner.store()
    }

    pub fn path(&self) -> PathBuf {
        db_path(self.dir.path())
    }
}

/// `backend.db` in `dir`.
pub fn db_path(dir: &Path) -> PathBuf {
    dir.join("backend.db")
}

/// A new store with [`TEST_KEY`].
pub fn temp_store() -> TempStore {
    let dir = tempfile::tempdir().expect("temp dir");
    let owner = StoreOwner::open(&db_path(dir.path()), &key(), "wa-test").expect("open the store");
    TempStore { dir, owner }
}

/// The name libtest knows a test by in the merged binary `tests/it.rs`: the tests that start
/// this binary again do it with `--exact <name>`. `module` is `module_path!()` of the calling
/// file (`it::kill`), `test` the function (`child`); the result is `kill::child`. The crate's own
/// name is not part of a libtest name.
pub fn child_filter(module: &str, test: &str) -> String {
    match module.split_once("::") {
        Some((_crate, path)) => format!("{path}::{test}"),
        None => test.to_owned(),
    }
}

/// Set in the child of [`in_own_process!`].
const OWN_PROCESS: &str = "KOURIER_OWN_PROCESS";

/// The libtest name of the test inside which an item was declared, from the item's
/// `type_name`: `it::kill::a_test::{{closure}}::here` (the body of a tokio test is a closure) is
/// `kill::a_test`. The crate (`it`, the merged binary) and the item itself are not part of a
/// libtest name.
pub fn test_name_of(item_path: &str) -> String {
    let mut parts: Vec<&str> = item_path.split("::").collect();
    parts.pop();
    if !parts.is_empty() {
        parts.remove(0);
    }
    parts.retain(|part| *part != "{{closure}}");
    parts.join("::")
}

/// The libtest name of the test that calls this, found from the path of an item declared in its
/// body: no string to keep in step with the function's name.
macro_rules! test_name {
    () => {{
        fn here() {}
        $crate::support::test_name_of(std::any::type_name_of_val(&here))
    }};
}
pub(crate) use test_name;

/// For a test that needs a process of its own; the first statement of the test:
/// `if support::in_own_process!() { return; }`. Under nextest every test is a process of its
/// own (`NEXTEST` is set) and this is `false`: run the body. Under `cargo test` (the fallback)
/// the tests of this binary are threads of ONE process: the call starts the binary again for this
/// test alone, is `true` once that passed (the caller returns) and panics with the child's output
/// when it failed; in the child it is `false`.
macro_rules! in_own_process {
    () => {
        $crate::support::in_own_process_named(&$crate::support::test_name!())
    };
}
pub(crate) use in_own_process;

/// What [`in_own_process!`] does with the name it found.
pub fn in_own_process_named(name: &str) -> bool {
    if std::env::var_os("NEXTEST").is_some() || std::env::var_os(OWN_PROCESS).is_some() {
        return false;
    }
    if let Err(why) = run_in_child(name) {
        panic!("{name} failed in a process of its own: {why}");
    }
    true
}

/// Starts this binary again for the one test `name` (`--exact`); the reason when it did not
/// pass exactly that test.
pub fn run_in_child(name: &str) -> Result<(), String> {
    let out = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .arg("--exact")
        .arg(name)
        .args(["--nocapture", "--test-threads=1"])
        .env(OWN_PROCESS, "1")
        .output()
        .expect("spawn the test again");
    let stdout = String::from_utf8_lossy(&out.stdout);
    child_verdict(out.status.success(), &stdout).map_err(|why| {
        format!(
            "{why} ({:?}):\n{stdout}{}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        )
    })
}

/// What the child's exit status and output say. A name that matches no test makes libtest exit
/// 0 after "running 0 tests"; that is no pass here.
fn child_verdict(success: bool, stdout: &str) -> Result<(), &'static str> {
    if !success {
        return Err("the child failed");
    }
    if !stdout.contains("test result: ok. 1 passed;") {
        return Err("the child did not pass exactly one test (a name that matches no test?)");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::offline::{Gate, REQUIRE_ENV, gate, unshare_or_skip};
    use super::{child_filter, child_verdict, test_name_of};

    /// The kill tests' log rule (`kill_log`) holds for the Wire's and the Cdn fake's logs: a line a killed child left
    /// unfinished is cut when the next child's `log_to` opens the log, and `cdn::read_log` never takes it (a cut `sha256`
    /// would name other bytes). Mutants: `log_to` opening the log plainly again; `read_log` over every line.
    #[test]
    fn a_line_a_killed_child_left_in_the_wire_or_cdn_log_is_cut_and_never_read() {
        let dir = tempfile::tempdir().expect("temp dir");
        let whole = "upload path=/mms/image direct_path=/v/a sha256=0a1b\n";
        let cdn_log = dir.path().join("cdn.log");
        std::fs::write(
            &cdn_log,
            format!("{whole}upload path=/mms/image direct_path=/v/b sha256=0"),
        )
        .expect("a torn log");
        let read = super::cdn::read_log(&cdn_log);
        assert_eq!(read.len(), 1, "the torn line is no upload");
        assert_eq!(read[0].sha256, "0a1b");
        super::cdn::Cdn::default().log_to(&cdn_log);
        assert_eq!(
            std::fs::read_to_string(&cdn_log).expect("read"),
            whole,
            "cut at the open"
        );

        let wire_log = dir.path().join("wire.log");
        std::fs::write(&wire_log, "ack id=3EB0A1\nmessage id=3EB0").expect("a torn log");
        super::wire::server().log_to(&wire_log);
        assert_eq!(
            std::fs::read_to_string(&wire_log).expect("read"),
            "ack id=3EB0A1\n",
            "cut at the open"
        );
    }

    #[test]
    fn the_child_filter_drops_the_crate() {
        assert_eq!(child_filter("it::kill", "child"), "kill::child");
        assert_eq!(child_filter("kill", "child"), "child");
    }

    #[test]
    fn the_test_name_comes_from_an_item_path() {
        assert_eq!(
            test_name_of("it::kill::a_test::{{closure}}::here"),
            "kill::a_test"
        );
        assert_eq!(test_name_of("it::open::a_test::here"), "open::a_test");
    }

    #[test]
    fn a_child_that_ran_no_test_is_no_pass() {
        assert!(child_verdict(true, "running 0 tests\ntest result: ok. 0 passed;").is_err());
        assert!(child_verdict(true, "test result: ok. 1 passed; 0 failed").is_ok());
        assert!(child_verdict(false, "test result: ok. 1 passed;").is_err());
    }

    /// Fix F12: where `unshare -rn` is unavailable a test skips, unless `KOURIER_REQUIRE_UNSHARE=1`
    /// (set by `cargo xtask ci`) says a skip is a failure. Both ways, on the decision itself.
    #[test]
    fn a_missing_unshare_is_a_skip_or_a_failure_as_the_run_asks() {
        assert_eq!(gate(true, None), Gate::Run);
        assert_eq!(gate(true, Some("1")), Gate::Run);
        assert_eq!(gate(false, None), Gate::Skip, "an ordinary run skips");
        assert_eq!(gate(false, Some("0")), Gate::Skip);
        assert_eq!(gate(false, Some("")), Gate::Skip);
        assert_eq!(gate(false, Some("1")), Gate::Fail, "ci asked for a failure");
    }

    /// The helper the tests call reads the variable ci sets: with it set and the namespace missing
    /// it panics, with it unset it returns false; where the namespace works it returns true.
    #[test]
    fn the_helper_reads_the_variable_ci_sets() {
        let available = super::offline::unshare_available();
        let required = std::env::var(REQUIRE_ENV).ok();
        if available {
            assert!(unshare_or_skip("a test"));
        } else if required.as_deref() == Some("1") {
            let panicked = std::panic::catch_unwind(|| unshare_or_skip("a test")).is_err();
            assert!(panicked, "a required namespace that is missing must fail");
        } else {
            assert!(!unshare_or_skip("a test"));
        }
    }
}
