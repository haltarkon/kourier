//! The integration tests of kourier-whatsapp, linked as ONE test binary (T3): every
//! `tests/<name>.rs` below is a module of it, so a test is `<name>::<test>` and the shared
//! `tests/support/` is compiled once.
//!
//! `autotests = false` in Cargo.toml: a NEW file in this directory is not a test until it is
//! declared here, and `every_test_file_is_declared` fails until it does.
//!
//! The tests that start this binary again as a child (`--exact <module>::<child>`, see
//! `support::child_filter`) are written for both runners: under nextest every test is a process
//! of its own, under `cargo test` (the fallback) they are threads of one.

mod support;

mod actor_real;
mod appstate_wire;
mod appsync;
mod builder;
mod calls;
mod device;
mod groups_flow;
mod groups_wire;
mod history_wire;
mod kill;
mod measure;
mod media_wire;
mod network;
mod open;
mod pending;
mod prepair;
mod protocol;
mod receive;
mod secret;
mod send_kill;
mod send_wire;
mod signal;
mod storage_contract_repro;
mod wam;
mod wire;
mod wire_kill;
mod wire_rig;

/// With `autotests = false` a forgotten `mod` line would silently drop a file's tests.
#[test]
fn every_test_file_is_declared() {
    let root = include_str!("it.rs");
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let mut missing = Vec::new();
    for entry in std::fs::read_dir(&dir).expect("tests dir") {
        let path = entry.expect("entry").path();
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        let module = match path.extension().and_then(|e| e.to_str()) {
            Some("rs") if name != "it.rs" => name.trim_end_matches(".rs"),
            None if path.is_dir() && name != "support" => name,
            _ => continue,
        };
        if !root.lines().any(|l| l.trim() == format!("mod {module};")) {
            missing.push(module.to_owned());
        }
    }
    assert!(
        missing.is_empty(),
        "not declared in tests/it.rs (their tests would not run): {missing:?}"
    );
}
