//! The integration tests of kourier-e2e, linked as ONE test binary (T3, 2026-10-14): every
//! `tests/<name>.rs` below is a module of it, so a test is `<name>::<test>` and the shared
//! `tests/support/` is compiled once. A binary of its own per file was 17 executables.
//!
//! `autotests = false` in Cargo.toml: a NEW file in this directory is not a test until it is
//! declared here, and `every_test_file_is_declared` fails until it is.
//!
//! The tests that start this binary again as a child (`--exact <module>::<child>`, see
//! `support::child_filter`) and those that read a process-wide log (`support::in_own_process`)
//! are written for both runners: under nextest every test is a process of its own, under
//! `cargo test` (the fallback) they are threads of one.

mod support;

mod signal_attachments;
mod signal_backfill;
mod signal_calls;
mod signal_contacts;
mod signal_core;
mod signal_files;
mod signal_group_send;
mod signal_groups;
mod signal_history_import;
mod signal_history_transfer;
mod signal_idle;
mod signal_link_tail;
mod signal_modifiers;
mod signal_roaming;
mod signal_robustness;
mod signal_safety;
mod signal_send;
mod signal_share;
mod signal_sync_state;
mod signal_transfers;

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
