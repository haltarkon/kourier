//! Helpers shared by the integration tests (and the `skeleton` example) of `kourier-signal`.

// Every test binary compiles this module and uses only part of it; so does the `skeleton`
// example, which needs none of the macros.
#![allow(dead_code, unused_macros, unused_imports)]

pub mod fake_core;
pub mod harness;

/// The name libtest knows a test by in the merged binary `tests/it.rs`: the tests that start
/// this binary again do it with `--exact <name>`. `module` is `module_path!()` of the calling
/// file (`it::groups`), `test` the function (`child_actor_process`); the result is
/// `groups::child_actor_process`. The crate's own name is not part of a libtest name.
pub fn child_filter(module: &str, test: &str) -> String {
    match module.split_once("::") {
        Some((_crate, path)) => format!("{path}::{test}"),
        None => test.to_owned(),
    }
}

/// Set in the child of [`in_own_process!`].
const OWN_PROCESS: &str = "KOURIER_OWN_PROCESS";

/// The libtest name of the test inside which an item was declared, from the item's
/// `type_name`: `it::robustness::a_test::{{closure}}::here` (the body of a tokio test is a
/// closure) is `robustness::a_test`. The crate (`it`, the merged binary) and the item itself
/// are not part of a libtest name.
pub fn test_name_of(item_path: &str) -> String {
    let mut parts: Vec<&str> = item_path.split("::").collect();
    parts.pop();
    if !parts.is_empty() {
        parts.remove(0);
    }
    parts.retain(|part| *part != "{{closure}}");
    parts.join("::")
}

/// The libtest name of the test that calls this (`robustness::a_test`), found from the path of
/// an item declared in its body: no string to keep in step with the function's name.
macro_rules! test_name {
    () => {{
        fn here() {}
        $crate::support::test_name_of(std::any::type_name_of_val(&here))
    }};
}
pub(crate) use test_name;

/// For a test that reads a process-wide log or needs anything else a second test of the
/// process would disturb; the first statement of the test:
/// `if support::in_own_process!() { return; }`. Under nextest every test is a process of its
/// own (`NEXTEST` is set) and this is `false`: run the body. Under `cargo test` the tests of this
/// binary are threads of ONE process (and the first global tracing subscriber wins): the call
/// starts the binary again for this test alone ([`run_in_child`]), is `true` once that passed
/// (the caller returns) and panics with the child's output when it failed; in the child it is
/// `false`. The name of the test comes from `test_name!`, so renaming the function cannot
/// leave the child running another (or no) test.
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
/// 0 after "running 0 tests"; that is no pass here (T3 review F3).
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
    use super::{child_filter, child_verdict, run_in_child, test_name_of};

    #[test]
    fn a_child_is_named_with_its_module() {
        assert_eq!(
            child_filter("it::groups", "child_actor_process"),
            "groups::child_actor_process"
        );
        assert_eq!(child_filter("it", "x"), "x");
    }

    #[test]
    fn a_test_finds_its_own_name() {
        assert_eq!(
            super::test_name!(),
            "support::tests::a_test_finds_its_own_name"
        );
        // A tokio test's body is a closure; a test of the binary's root has no module.
        assert_eq!(
            test_name_of("it::robustness::a_test::{{closure}}::here"),
            "robustness::a_test"
        );
        assert_eq!(test_name_of("it::a_root_test::here"), "a_root_test");
    }

    #[test]
    fn a_child_that_ran_no_test_is_not_a_pass() {
        // libtest exits 0 after "running 0 tests" for a name that matches nothing.
        let why = run_in_child("support::tests::there_is_no_such_test").expect_err("no pass");
        assert!(why.contains("exactly one test"), "{why}");
        let why = run_in_child("there_is_no_such_test").expect_err("no pass");
        assert!(why.contains("exactly one test"), "{why}");
    }

    #[test]
    fn a_child_that_passed_one_test_is_a_pass() {
        run_in_child("support::tests::a_test_that_passes").expect("a pass");
    }

    #[test]
    fn a_test_that_passes() {}

    #[test]
    fn the_verdict_wants_exactly_one_passed_test() {
        let one = "running 1 test\ntest x ... ok\n\ntest result: ok. 1 passed; 0 failed; 0 ignored";
        let none = "running 0 tests\n\ntest result: ok. 0 passed; 0 failed; 0 ignored";
        assert_eq!(child_verdict(true, one), Ok(()));
        assert!(child_verdict(true, none).is_err());
        assert!(child_verdict(false, one).is_err());
        assert!(child_verdict(true, "").is_err());
    }
}
