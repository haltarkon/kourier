//! The runbook (`docs/telegram-run.md`) against the tool: every command it tells the lead to type is
//! one the tool takes, every word of the verdict it names is one the tool prints, the bounds it states
//! are the tool's own, and the file is fit for the public source archive (no em dash, no private path,
//! no Telegram host name). A sentence the tool stops being true to fails here, not in the user's go.

use crate::tg_run::app_names::DIAL_ENV;
use crate::tg_run::connect::Bounds;
use crate::tg_run::stage::{SENTINEL_TAIL, build_args};
use crate::tg_run::{API_FILE, CONNECT_COMMAND, DEFAULT_PROFILE, KEEPER_DIR};

const RUNBOOK: &str = "docs/telegram-run.md";

fn runbook() -> String {
    std::fs::read_to_string(crate::root().join(RUNBOOK))
        .unwrap_or_else(|e| panic!("{RUNBOOK}: {e}"))
}

/// The text with its white space collapsed: lines wrap, a fact is looked for across the wrap.
fn flat(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Mutant: a fact removed from the runbook (on the server's copy) fails this test.
#[test]
fn the_runbook_states_every_command_word_bound_and_rule_of_the_tool() {
    let flat = flat(&runbook());
    let build = format!("cargo {}", build_args(false).join(" "));
    let first_qr = Bounds::REAL.first_qr.as_secs();
    let exit = Bounds::REAL.exit.as_secs();
    let answer = Bounds::REAL.answer.as_secs();
    let stated = [
        // The build first, with no go, and the connect after it.
        format!("kcap -t 5400 {build}"),
        // The one constant the preflight's refusal tells too (D-4).
        CONNECT_COMMAND.to_owned(),
        "--profile <name>".to_owned(),
        format!("another profile than `{DEFAULT_PROFILE}`"),
        "--no-build".to_owned(),
        // The address list and its marker.
        "cargo xtask tg-run ranges".to_owned(),
        "VERIFIED".to_owned(),
        "UNVERIFIED".to_owned(),
        "list-sha256:".to_owned(),
        "--allow-unverified-ranges".to_owned(),
        "--allow-unencrypted-disk".to_owned(),
        // The user's own file, and the places.
        format!("~/.local/share/{KEEPER_DIR}/{API_FILE}"),
        "srv -f target/tdlib 'kcap -t 900 cargo xtask tdlib build'".to_owned(),
        // The rehearsal.
        "srv 'kcap -t 5400 cargo xtask tg-run rehearse'".to_owned(),
        "REHEARSAL PASS".to_owned(),
        "unshare -rn".to_owned(),
        "KOURIER_DEV_REHEARSAL=telegram".to_owned(),
        "PR_SET_PDEATHSIG".to_owned(),
        "disk not checked".to_owned(),
        // Undoing.
        "cargo xtask tg-run wipe --yes-delete-the-login".to_owned(),
        // The confirmation.
        "the environment of the app it starts, and for a moment in the input of the `sha256sum` process"
            .to_owned(),
        "tg-run/dial-go.json".to_owned(),
        "(two minutes)".to_owned(),
        "KOURIER_DEV_QR_URL=0".to_owned(),
        "a number tail that no phone number ends in (fifteen zeros)".to_owned(),
        "mode=refusing reason=no-confirmation".to_owned(),
        // What PASS means: the words the report prints.
        "mode=telegram reason=confirmed".to_owned(),
        "confirmation: taken by the app".to_owned(),
        "confirmation: none written".to_owned(),
        "`QR codes shown`: at least 1".to_owned(),
        "raw needles in the app's stderr: 0".to_owned(),
        "dials >= 1".to_owned(),
        "dial_failures < dials".to_owned(),
        "rule == 0".to_owned(),
        "port == 0".to_owned(),
        "address_type == 0".to_owned(),
        "EVERY lock-down word is `on`".to_owned(),
        // R-2: the gate is judged per account actor, and a dropped gate is not seen.
        "every account actor that told its lock-down word has a final counts line".to_owned(),
        "a gate that was dropped without its stop line (a panic)".to_owned(),
        "exactly ONE gate session".to_owned(),
        "sends: not measured".to_owned(),
        // What to send back (H-12): the report's words, counts and times, never a value.
        "the profile's name, the servers, the build commit and the walk's counts, words and times (no value, name or number)"
            .to_owned(),
        // The bounds, from the tool.
        format!("waits at most {first_qr} s for the first login QR code"),
        format!("waits at most {exit} s (then kills it)"),
        format!("Each answer has {answer} s"),
    ];
    for fact in &stated {
        assert!(flat.contains(fact.as_str()), "the runbook states {fact:?}");
    }
    // The fifteen zeros are the tool's sentinel tail, and the claim is about the app's variable.
    assert_eq!(SENTINEL_TAIL.len(), 15);
    assert!(SENTINEL_TAIL.bytes().all(|b| b == b'0'));
    assert!(flat.contains(DIAL_ENV), "the runbook names {DIAL_ENV}");
    // `sends: 0` is no claim the tool makes any more (H-4).
    assert!(!flat.contains("sends: 0"), "the runbook says `sends: 0`");
    // The claims the verdict cannot back (R-2, D-2): no "every helper that started", no exactly one code.
    assert!(
        !flat.contains("every helper that started"),
        "the runbook claims the helper starts are counted"
    );
    assert!(
        !flat.contains("QR codes shown: 1"),
        "a refreshed code passes with more than one shown"
    );
}

/// The freshness the runbook states is the app's own.
#[test]
fn the_runbooks_two_minutes_are_the_apps_freshness() {
    let app = std::fs::read_to_string(crate::root().join("crates/kourier-app/src/telegram.rs"))
        .expect("the app's Telegram file");
    assert!(
        app.lines()
            .any(|l| l.starts_with("const DIAL_FRESH_MS: u64 = 120_000;")),
        "the app's freshness is no longer two minutes: change the runbook"
    );
}

/// The file goes into the public source archive: no em dash, no private path, no Telegram host. The
/// host names are built from parts (the lane's source checks scan this directory).
#[test]
fn the_runbook_is_fit_for_the_public_archive() {
    let text = runbook();
    assert!(!text.contains('\u{2014}'), "an em dash in the runbook");
    let who = crate::dist::Who {
        home: "/home/nobody-at-all".to_owned(),
        user: "nobody-at-all".to_owned(),
    };
    for (i, line) in text.lines().enumerate() {
        assert!(
            crate::dist::private_path_hits(line, &who).is_empty(),
            "line {}: a private path",
            i + 1
        );
    }
    let lower = flat(&text).to_ascii_lowercase();
    for host in [
        ["telegram", ".org"].concat(),
        ["telegram", ".me"].concat(),
        ["telegra", ".ph"].concat(),
        ["t", ".me"].concat(),
    ] {
        assert!(
            !lower.contains(&host),
            "a Telegram host name in the runbook"
        );
    }
    // No value of any kind: the only digits the runbook shows are counts and bounds, and the API file's
    // two lines are shown as placeholders.
    assert!(text.contains("id=<the digits>") && text.contains("hash=<32 lower-case hex digits>"));
}
