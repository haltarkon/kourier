//! The runbook (`docs/threema-run.md`) against the tool: every command it tells the lead to type is one
//! the tool takes, every step and every stop word it names is one the tool has, the bounds it states are
//! the tool's own, and the file is fit for the public source archive (no em dash, no private path, no
//! Threema host name, no ID and no digest). A sentence the tool stops being true to fails here, not in
//! the user's go.

use crate::threema_run::judge::{HOLD_RULES, PHONE_HOLDS, TOLERATED_HOLDS};
use crate::threema_run::preflight::MAXERROR_LIMIT_US;
use crate::threema_run::steps::{STEP_WAIT_MAX_SECS, STEPS, StepTiming};
use crate::threema_run::supervise::{
    DROP_WAIT, FIRST_ANSWER_WITHIN, ID_WAIT, KILL_GRACE, LINK_END_WAIT,
};
use crate::threema_run::{ALLOW_FILE, BACKUPS_DIR, DEFAULT_PROFILE, KEEPER_DIR, USAGE};

const RUNBOOK: &str = "docs/threema-run.md";

fn runbook() -> String {
    std::fs::read_to_string(crate::root().join(RUNBOOK))
        .unwrap_or_else(|e| panic!("{RUNBOOK}: {e}"))
}

/// The text with its white space collapsed: lines wrap, a fact is looked for across the wrap.
fn flat(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn source(path: &str) -> String {
    std::fs::read_to_string(crate::root().join(path)).unwrap_or_else(|e| panic!("{path}: {e}"))
}

/// Mutant: a fact removed from the runbook (on the server's copy) fails this test.
#[test]
fn the_runbook_states_every_command_flag_word_and_bound_of_the_tool() {
    let flat = flat(&runbook());
    let secs = |d: std::time::Duration| d.as_secs();
    let real = StepTiming::REAL;
    let stated = [
        // The build with no go, and the commands, as the lead and the user type them.
        "kcap -t 5400 cargo build -p kourier".to_owned(),
        "kcap -t 900 cargo xtask threema-run start --confirmed-by-user --for-link".to_owned(),
        "kcap -t 900 cargo xtask threema-run start --confirmed-by-user".to_owned(),
        "target/debug/xtask threema-run id".to_owned(),
        "target/debug/xtask threema-run answer".to_owned(),
        "cargo xtask threema-run step link".to_owned(),
        "cargo xtask threema-run step reconnect".to_owned(),
        "cargo xtask threema-run stop".to_owned(),
        "cargo xtask threema-run backup [--to <dir>]".to_owned(),
        "cargo xtask threema-run wipe --yes-delete-the-login [--profile <name>]".to_owned(),
        "step passive --seconds 600"
            .replace("step passive", "`passive")
            .replace("600", "600`"),
        "srv 'kcap -t 5400 cargo xtask threema-run rehearse'".to_owned(),
        "REHEARSAL PASS: <N> checks in <S> s".to_owned(),
        "unshare -rn".to_owned(),
        "KOURIER_DEV_REHEARSAL=threema".to_owned(),
        "--profile <name>".to_owned(),
        "--after-halt".to_owned(),
        "--allow-schema-change".to_owned(),
        "--allow-unencrypted-disk".to_owned(),
        "--no-build".to_owned(),
        "--keep".to_owned(),
        "--force".to_owned(),
        format!("the default profile is `{DEFAULT_PROFILE}`"),
        format!("~/.local/share/{KEEPER_DIR}/"),
        format!("`{ALLOW_FILE}`"),
        format!("The profile name `{BACKUPS_DIR}` is reserved."),
        format!("`{BACKUPS_DIR}/<name>-<UTC time>`"),
        "taken (8 characters)".to_owned(),
        "DO NOT CLICK IN THE WINDOW IN ITS FIRST SECONDS".to_owned(),
        // The bounds, from the tool.
        format!("The wait is {} minutes.", secs(ID_WAIT) / 60),
        format!(
            "It has {} s to answer on its dev control",
            secs(FIRST_ANSWER_WITHIN)
        ),
        format!("(up to {} s)", secs(LINK_END_WAIT)),
        format!("link failed after its hello ({} s)", secs(DROP_WAIT)),
        format!("killed after {} s", secs(KILL_GRACE)),
        format!("A call watches at most {STEP_WAIT_MAX_SECS} s"),
        format!("The scan has {} s.", secs(real.qr_wait)),
        format!(
            "the TEST phone locked for {} minutes",
            secs(real.sleep_total) / 60
        ),
        format!(
            "its error bound must be under {} s",
            MAXERROR_LIMIT_US / 1_000_000
        ),
        // What the tool says of the two ends and of itself.
        "This ENDS the link: do you want that now?".to_owned(),
        "Threema is switched off in the profile".to_owned(),
        "Between runs the profile does not connect to Threema.".to_owned(),
    ];
    for fact in &stated {
        assert!(flat.contains(fact.as_str()), "the runbook states {fact:?}");
    }
    // Every command and option the runbook shows is in the tool's own usage.
    for word in [
        "start",
        "id",
        "answer",
        "step",
        "steps",
        "state",
        "report",
        "stop",
        "backup",
        "wipe",
        "rehearse",
        "--confirmed-by-user",
        "--for-link",
        "--after-halt",
        "--allow-schema-change",
        "--no-build",
        "--allow-unencrypted-disk",
        "--yes-delete-the-login",
        "--to <dir>",
        "--force",
        "--keep",
    ] {
        assert!(USAGE.contains(word), "the usage names {word}");
    }
}

/// Every step the tool has is named, and the table has no step the runbook lacks.
#[test]
fn the_runbook_names_every_step_of_the_tool() {
    let text = flat(&runbook());
    for def in STEPS {
        let n = def.name;
        assert!(
            [format!("`{n}`"), format!("`{n} "), format!("step {n}`")]
                .iter()
                .any(|shape| text.contains(shape)),
            "the runbook names the step {n}"
        );
    }
    assert_eq!(STEPS.len(), 12, "a new step needs its place in the runbook");
}

/// Every stop word the runbook names is one the judge (or the supervisor's time limit) can say, and every
/// hold word and its verdict are the judge's own.
#[test]
fn the_runbooks_stop_words_are_the_judges() {
    let flat = flat(&runbook());
    let code = format!(
        "{}{}",
        source("xtask/src/threema_run/judge.rs"),
        source("xtask/src/threema_run/supervise.rs")
    );
    for kind in [
        "link-refused",
        "list-not-taken",
        "link-failed-after-hello",
        "needs-action",
        "restricted",
        "logged-out",
        "close-error",
        "untrusted",
        "not-registered",
        "first-row-refused",
        "clock",
        "unasked",
        "follower-wrote",
        "leader-burst",
        "transmit",
        "unhandled-other",
        "line-unreadable",
        "socket-loop",
        "lost",
        "twice",
        "phone-warning",
        "stopped",
        "error",
        "exited",
        "app-not-up",
        "count-lines-missing",
        "list-not-removed",
        "app-readable",
    ] {
        assert!(
            flat.contains(&format!("`{kind}`")),
            "the runbook names the stop {kind}"
        );
        assert!(
            code.contains(&format!("\"{kind}\"")),
            "the judge says {kind}"
        );
    }
    for (shape, template) in [
        ("`close-<code>`", "close-{code}"),
        ("`unhandled-<kind>`", "unhandled-{kind}"),
        ("`held-<rule>`", "held-{rule}"),
    ] {
        assert!(flat.contains(shape), "the runbook names {shape}");
        assert!(code.contains(template), "the judge makes {template}");
    }
    for rule in HOLD_RULES {
        assert!(
            flat.contains(&format!("`{rule}`")),
            "the runbook names the hold {rule}"
        );
    }
    let phone = PHONE_HOLDS.map(|r| format!("`{r}`")).join(", ");
    let known: Vec<String> = TOLERATED_HOLDS
        .iter()
        .filter(|r| !PHONE_HOLDS.contains(r))
        .map(|r| format!("`{r}`"))
        .collect();
    assert_eq!(
        known,
        ["`inbound-known`"],
        "the tolerated holds beside the phone's"
    );
    assert!(
        flat.contains(&format!(
            "The two holds only the phone resolves ({phone}) and a second copy of a message already known ({}) do not stop it",
            known.join(", ")
        )),
        "the runbook says which holds do not stop"
    );
    assert_eq!(TOLERATED_HOLDS.len(), 3);
    // The two reflected words stop and the runbook says so by name.
    for stops in [
        "inbound-reflected-zone-failed",
        "inbound-reflected-queue-full",
    ] {
        assert!(!TOLERATED_HOLDS.contains(&stops));
        assert!(flat.contains(&format!("`{stops}`")), "{stops}");
    }
}

/// The step-aside the runbook describes is the backend's: its pause and its spacing are constants there.
#[test]
fn the_runbooks_step_aside_is_the_backends() {
    let state = source("crates/kourier-threema/src/conn/state.rs");
    assert!(
        state
            .lines()
            .any(|l| l.contains("YIELD_PAUSE: Duration = Duration::from_secs(2 * 60)")),
        "the backend's pause is no longer two minutes: change the runbook"
    );
    let leader = source("crates/kourier-threema/src/leader/mod.rs");
    assert!(
        leader
            .lines()
            .any(|l| l.contains("YIELD_EVERY: Duration = Duration::from_secs(15 * 60)")),
        "the backend's spacing is no longer 15 minutes: change the runbook"
    );
    let flat = flat(&runbook());
    assert!(flat.contains("closes its socket with 1000 ONCE and pauses two minutes"));
    assert!(flat.contains("at most once every 15 minutes"));
}

/// The file goes into the public source archive: no em dash, no private path, no Threema host, no ID, no
/// digest. The host names are built from parts (the lane's source checks scan this directory).
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
        ["threema", ".ch"].concat(),
        ["threema", ".com"].concat(),
        ["threema", ".chat"].concat(),
        ["threema", "-", "gateway"].concat(),
        ["https", "://"].concat(),
    ] {
        assert!(
            !lower.contains(&host),
            "a Threema host name or a URL in the runbook: {host}"
        );
    }
    assert!(
        !crate::threema_run::holds_a_digest(&text),
        "a run of 64 hex digits in the runbook"
    );
    // No token with the shape of an ID: eight capitals and digits with a digit in them (the
    // rehearsal's words are longer or lower-case).
    for token in text.split(|c: char| !c.is_ascii_alphanumeric() && c != '*') {
        let shaped = crate::threema_run::looks_like_an_id(token)
            && token.bytes().any(|b| b.is_ascii_digit())
            && token.bytes().all(|b| !b.is_ascii_lowercase());
        assert!(!shaped, "a token with the shape of an ID in the runbook");
    }
}
