//! TR R0a fix G-1 (review F1): the tripwire of the TCP dialer covers THIS test binary. The dialer's
//! permission check is asked through `kourier_telegram::offline::a_dial_is_permitted`, which opens
//! no socket and names no mode: outside the offline child (`unshare -rn`, every address routed to
//! `lo`) not even an address of the list on a listed port is permitted; inside it the same address
//! is, and an address or a port outside the list still is not.
//!
//! The binary has the crate's `test-support` feature (the door and the tripwire) from the crate's own
//! self dev-dependency (`Cargo.toml`), so this module runs under `-p kourier-telegram` too.

use std::net::SocketAddr;
use std::time::Duration;

use kourier_telegram::offline::{
    a_dial_is_permitted, a_listed_address, in_offline_child, offline_available, run_self_offline,
};

/// Set in the child (by the parent); an unset variable makes the child's test a no-op.
const CASE_ENV: &str = "KOURIER_TG_DIAL_PROBE_CHILD";
/// `1`: a missing `unshare -rn` fails the test instead of skipping it.
const REQUIRE_UNSHARE_ENV: &str = "KOURIER_REQUIRE_UNSHARE";
/// What the child prints when every assertion held.
const MARKER: &str = "PROBE ok: listed=permitted outside_the_list=refused";

/// The child's path as `--exact` names it: this module's path without the binary's name.
fn child_name() -> String {
    let here = module_path!();
    let inside = here.split_once("::").map_or(here, |(_, rest)| rest);
    format!("{inside}::the_probe_child")
}

/// Outside the offline child the TCP dialer's permission is refused for an address of the list on
/// a listed port: the tripwire is armed in this binary, whatever the process inherits. Mutant: the
/// tripwire's cfg back to `cfg(test)` (it is then armed only in the crate's own unit tests).
#[test]
fn a_listed_dial_is_not_permitted_outside_the_offline_child() {
    assert!(
        !in_offline_child(),
        "this test is for the plain test process"
    );
    assert!(
        !a_dial_is_permitted(a_listed_address()),
        "a listed dial outside the offline child was permitted: the tripwire is not armed here"
    );
}

/// The parent half of the inside case: the child under `unshare -rn`, its exit and its marker.
#[test]
fn a_listed_dial_is_permitted_inside_the_offline_child() {
    if !offline_available() {
        assert!(
            !std::env::var(REQUIRE_UNSHARE_ENV).is_ok_and(|v| v == "1"),
            "`unshare -rn` (or `ip`) is unavailable and {REQUIRE_UNSHARE_ENV}=1"
        );
        println!("SKIPPED: `unshare -rn` or `ip` is unavailable: no dial is judged outside it");
        return;
    }
    let run = run_self_offline(&child_name(), &[(CASE_ENV, "1")], Duration::from_secs(60))
        .expect("start the offline child");
    assert!(
        run.status.success() && run.output.contains("1 passed"),
        "the offline child failed ({:?}):\n{}",
        run.status,
        run.output
    );
    assert!(
        run.output.contains(MARKER),
        "the offline child did not reach its end:\n{}",
        run.output
    );
}

/// The child's body; a no-op in the plain test process. It only asks the permission: no socket is
/// opened here.
#[test]
fn the_probe_child() {
    if std::env::var_os(CASE_ENV).is_none() {
        return;
    }
    assert!(
        in_offline_child(),
        "not in the offline child: nothing is judged"
    );
    let listed = a_listed_address();
    assert!(
        a_dial_is_permitted(listed),
        "a listed dial was not permitted inside the offline child"
    );
    // Outside the list: a private address, and the listed address on a port the gate never forwards.
    assert!(!a_dial_is_permitted(SocketAddr::from(([10, 0, 0, 1], 443))));
    assert!(!a_dial_is_permitted(SocketAddr::new(listed.ip(), 22)));
    println!("{MARKER}");
}
