//! The integration tests of kourier-core, linked as ONE test binary (T3, 2026-10-14): every
//! `tests/<name>.rs` below is a module of it, so a test is `<name>::<test>` and the shared
//! `tests/support/` is compiled once. A binary of its own per file was 36 executables of
//! 160 MB each.
//!
//! `autotests = false` in Cargo.toml: a NEW file in this directory is not a test until it is
//! declared here, and `every_test_file_is_declared` fails until it is.
//!
//! The kill and crash tests start this binary again as a child (`--exact <module>::<child>`,
//! see `support::child_filter`). Under nextest every test is a process of its own; under
//! `cargo test` (the fallback) they are threads of one: nothing here may keep global state
//! (environment variables, statics) that another test could see.

mod support;

mod account_hold;
mod account_states;
mod archive;
mod attachments;
mod attachments_crash;
mod budget;
mod calls;
mod calls_attack;
mod cancel_upload;
mod chat_media;
mod cloud_ids;
mod crash;
mod delete_for_me;
mod derive_count;
mod expiry_policy;
mod files;
mod followups;
mod followups_i;
mod group_invite;
mod group_page;
mod history_rules;
mod history_rules_review;
mod hostile;
mod ingest;
mod lifecycle;
mod local_deletes;
mod lost_history;
mod merge_cost;
mod message_actions;
mod modifiers;
mod networks;
mod one_clock;
mod outbox;
mod outbox_crash;
mod people;
mod perf_marks;
mod profile_mark;
mod protected_chats;
mod quiet_notices;
mod reach_out;
mod reaction_removal;
mod receipts;
mod remove_derive;
mod remove_unlink;
mod restore;
mod restore_fake;
mod restore_start;
mod revisions;
mod roaming;
mod roaming_clear;
mod robustness;
mod safety_number;
mod save;
mod scale;
mod schema_16;
mod schema_17;
mod schema_18;
mod schema_19;
mod schema_20;
mod schema_21;
mod send_contact;
mod send_rules;
mod stop_grace;

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
