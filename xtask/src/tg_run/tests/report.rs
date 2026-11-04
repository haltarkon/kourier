//! The readers of the app's log, THE VERDICT and the redaction (X21, X22, X23): pure, over lines in the
//! shape the app logs them (the message first, then `key=value` fields).

use crate::tg_run::report::{
    COUNT_KEYS, Confirmation, ExitHow, LogFacts, Report, RunKind, Step, Verdict, Walk,
    expected_dialer, kind_word, lines, parse_status_memory, read_log, redact, step_word, verdict,
};

const LEAD: &str = "2026-11-01T10:00:00.000000Z  INFO";

pub(super) fn dialer_line(mode: &str, reason: &str) -> String {
    format!("{LEAD} kourier_app::telegram: telegram dialer mode=\"{mode}\" reason=\"{reason}\"")
}

pub(super) fn lockdown_line(word: &str) -> String {
    format!("{LEAD} kourier_telegram::actor: telegram helper lock-down lockdown=\"{word}\"")
}

/// A counts line with `values` for the keys named and 0 for the others.
pub(super) fn counts_line(when: &str, values: &[(&str, u64)]) -> String {
    let numbers: Vec<String> = COUNT_KEYS
        .iter()
        .map(|k| {
            let n = values
                .iter()
                .find(|(key, _)| key == k)
                .map_or(0, |(_, n)| *n);
            format!("{k}={n}")
        })
        .collect();
    format!(
        "{LEAD} kourier_telegram::gate: telegram gate counts when=\"{when}\" {}",
        numbers.join(" ")
    )
}

/// A clean stop line of a real run: 3 accepted, 2 dials, none failed, nothing refused.
pub(super) fn good_stop() -> String {
    counts_line("stop", &[("accepted", 3), ("dials", 2), ("bytes_up", 900)])
}

/// A login step event: the step word and the kind word the dev control names, not yet followed by its
/// `login_qr` event.
pub(super) fn step(word: &str, kind: &str) -> Step {
    Step::new(word, kind)
}

/// A QR code the app showed: the `qr` step of the kind `qr` and the `login_qr` event with `shown`.
pub(super) fn qr_step() -> Step {
    Step {
        shown: true,
        ..Step::new("qr", "qr")
    }
}

/// A walk that did what a connect should for a CONFIRMED order: one shown QR code, the close answered,
/// no account, quit, the app took the confirmation.
pub(super) fn good_walk() -> Walk {
    Walk {
        steps: vec![qr_step()],
        qr_after_ms: Some(4210),
        closed: Some(true),
        accounts: Some(0),
        memory_kb: Some((204_800, 262_144)),
        exit: ExitHow::Quit(0),
        confirmation: Some(Confirmation::TakenByApp),
        ..Walk::default()
    }
}

/// The same walk of an order that is `confirmed` or not: the other writes no file.
pub(super) fn good_walk_for(confirmed: bool) -> Walk {
    Walk {
        confirmation: Some(if confirmed {
            Confirmation::TakenByApp
        } else {
            Confirmation::NoneWritten
        }),
        ..good_walk()
    }
}

fn facts(lines: &[String]) -> LogFacts {
    read_log(&lines.join("\n"))
}

/// What a real, confirmed connect leaves in the log.
fn real_log() -> Vec<String> {
    vec![
        dialer_line("telegram", "confirmed"),
        lockdown_line("on"),
        counts_line("running", &[("accepted", 1), ("dials", 1)]),
        good_stop(),
    ]
}

fn failures(v: &Verdict) -> String {
    v.failures.join("; ")
}

// ---- X21 ------------------------------------------------------------------------------------------

/// X21: the gate's counts are read per line with their numbers, a line that is missing stays missing
/// (never a 0), a line with a number missing is unreadable, not a 0. Mutant: a missing number read as
/// 0.
#[test]
fn the_gate_counts_are_read_per_start_and_missing_ones_are_not_measured() {
    let f = facts(&real_log());
    assert_eq!(f.dialer, [("telegram".to_owned(), "confirmed".to_owned())]);
    assert_eq!(f.lockdown, ["on"]);
    assert_eq!(f.counts.len(), 2);
    assert_eq!(f.counts[0].when, "running");
    assert_eq!(f.counts[1].when, "stop");
    assert_eq!(f.counts[1].get("accepted"), Some(3));
    assert_eq!(f.counts[1].get("dials"), Some(2));
    assert_eq!(f.counts[1].get("bytes_up"), Some(900));
    assert_eq!(f.counts[1].get("nonsense"), None);
    assert_eq!(f.counts_unreadable, 0);

    // No counts line at all: nothing is read, and the report says "not measured", never 0.
    let none = facts(&[dialer_line("telegram", "confirmed"), lockdown_line("on")]);
    assert!(none.counts.is_empty());
    let report = |log: &LogFacts, kind| {
        let walk = good_walk();
        let v = verdict(&walk, log, true, kind);
        lines(&Report {
            profile: "r0",
            env: "test",
            build: Some("a1b2c3d4e5"),
            confirmed: true,
            kind,
            walk: &walk,
            log,
            verdict: &v,
        })
        .join("\n")
    };
    let text = report(&none, RunKind::Real);
    assert!(text.contains("gate counts: not measured"), "{text}");
    assert!(text.contains("connect: FAIL"), "{text}");
    assert!(!text.contains("accepted=0"), "{text}");

    // A counts line that lacks a number is unreadable, not read with a 0 for it.
    let broken = counts_line("stop", &[("dials", 2)]).replace("rule=0 ", "");
    assert!(!broken.contains("rule="), "{broken}");
    let f = facts(&[broken]);
    assert!(f.counts.is_empty(), "a missing number was read as 0");
    assert_eq!(f.counts_unreadable, 1);
    // A number that is no number is unreadable too.
    let f = facts(&[counts_line("stop", &[("dials", 2)]).replace("rule=0", "rule=many")]);
    assert!(f.counts.is_empty() && f.counts_unreadable == 1);

    // Words outside the vocabulary are read as `unknown`: the report prints no word it does not know.
    let f = facts(&[
        dialer_line("+12345678901", "something-new"),
        lockdown_line("a name"),
    ]);
    assert_eq!(f.dialer, [("unknown".to_owned(), "unknown".to_owned())]);
    assert_eq!(f.lockdown, ["unknown"]);

    // The finding for a stale list, with its count.
    let stale = [
        dialer_line("telegram", "confirmed"),
        lockdown_line("on"),
        counts_line("stop", &[("dials", 2), ("rule", 3)]),
    ];
    let v = verdict(&good_walk(), &facts(&stale), true, RunKind::Real);
    assert!(!v.pass);
    assert!(
        v.findings
            .iter()
            .any(|f| f.contains("refused 3 connection(s) by its address list: the list is stale")),
        "{:?}",
        v.findings
    );
}

// ---- X22 ------------------------------------------------------------------------------------------

/// The samples of what must never reach a report, assembled at run time (the lane's source check scans
/// this file for Telegram's hosts and API literals).
fn secret_samples() -> Vec<(String, String)> {
    let host = ["t", ".me/"].concat();
    let login = ["tg", "://"].concat();
    let digits = |n: usize| "7".repeat(n);
    vec![
        (
            format!("user tg:{} sent", digits(10)),
            "tg:[digits]".to_owned(),
        ),
        (
            format!("group tg:-{} sent", digits(13)),
            "tg:[digits]".to_owned(),
        ),
        (format!("tg:{}", digits(5)), "tg:[digits]".to_owned()),
        (format!("tg:{}", digits(19)), "tg:[digits]".to_owned()),
        (
            format!("owner pn:{} here", digits(12)),
            "pn:[digits]".to_owned(),
        ),
        (
            "phone +380501234567 here".to_owned(),
            "+[number]".to_owned(),
        ),
        (
            format!("open {login}login?token=AbCdEf123 now"),
            "[link]".to_owned(),
        ),
        (
            format!("see https://{host}joinchat/AAAA now"),
            "[link]".to_owned(),
        ),
        (format!("see {host}somebody now"), "[link]".to_owned()),
        (format!("nonce {} end", "ab".repeat(32)), "[hex]".to_owned()),
    ]
}

/// X22: nothing secret reaches a report: every shape is cut, in a log line and in a report line.
/// Mutant: the `tg:` shape dropped.
#[test]
fn nothing_secret_reaches_a_report() {
    for (sample, marker) in secret_samples() {
        let out = redact(&sample);
        assert!(out.contains(&marker), "{sample} became {out}");
        // The secret part is gone: no run of seven digits, no hex run, no link, no token.
        let digits = out.bytes().fold((0, 0), |(run, best), b| {
            let run = if b.is_ascii_digit() { run + 1 } else { 0 };
            (run, best.max(run))
        });
        assert!(digits.1 < 5, "{sample} became {out}");
        assert!(
            !out.contains("AbCdEf123") && !out.contains("joinchat"),
            "{out}"
        );
        assert!(!out.contains(&"ab".repeat(32)), "{out}");
    }
    // Harmless text stays.
    for fine in [
        "telegram dialer mode=\"telegram\" reason=\"confirmed\"",
        "accepted=1234567 dials=2 bytes_up=99999999",
        "tg:1234 is too short to be an id",
        "a hash of 63 hex digits stays",
    ] {
        assert_eq!(redact(fine), fine);
    }
    // The 63- and the 65-digit runs are not 64.
    let sixty_three = "a".repeat(63);
    assert_eq!(redact(&sixty_three), sixty_three);
    let sixty_five = "a".repeat(65);
    assert_eq!(redact(&sixty_five), sixty_five);
    // A log line goes through the same cut before it is written, so the report's lines are clean even
    // when the profile's NAME is hostile.
    let walk = good_walk();
    let log = facts(&real_log());
    let v = verdict(&walk, &log, true, RunKind::Real);
    let name = format!("tg:{}", "7".repeat(10));
    let text = lines(&Report {
        profile: &name,
        env: "test",
        build: None,
        confirmed: true,
        kind: RunKind::Real,
        walk: &walk,
        log: &log,
        verdict: &v,
    })
    .join("\n");
    assert!(!text.contains("7777777"), "{text}");
}

// ---- X23 ------------------------------------------------------------------------------------------

fn pass(walk: &Walk, log: &[String], confirmed: bool, kind: RunKind) -> Verdict {
    verdict(walk, &facts(log), confirmed, kind)
}

fn expect_fail(v: &Verdict, why: &str) {
    assert!(!v.pass, "passed, but should fail on: {why}");
    assert!(
        v.failures.iter().any(|f| f.contains(why)),
        "wanted `{why}`, got: {}",
        failures(v)
    );
}

/// X23: the verdict is a pure function of the events, the state and the lines, and the order. Mutants:
/// a missing counts line accepted in a real run; the mode compare dropped.
#[test]
fn connect_passes_only_on_a_qr_closed_without_a_link() {
    // A real, confirmed run that did everything: PASS.
    let real = pass(&good_walk(), &real_log(), true, RunKind::Real);
    assert!(real.pass, "{}", failures(&real));
    assert!(real.findings.is_empty());
    assert_eq!(real.word(), "PASS");

    // The rehearsal, unconfirmed: the app refused to dial, nothing of the real backend ran. PASS, with
    // the counts "not measured (rehearsal)".
    let refusing = [dialer_line("refusing", "no-confirmation")];
    let r = pass(&good_walk_for(false), &refusing, false, RunKind::Rehearsal);
    assert!(r.pass, "{}", failures(&r));
    let walk = good_walk_for(false);
    let log = facts(&refusing);
    let text = lines(&Report {
        profile: "p",
        env: "test",
        build: None,
        confirmed: false,
        kind: RunKind::Rehearsal,
        walk: &walk,
        log: &log,
        verdict: &r,
    })
    .join("\n");
    assert!(
        text.contains("gate counts: not measured (rehearsal)"),
        "{text}"
    );
    assert!(
        text.contains("lock-down: not measured (rehearsal)"),
        "{text}"
    );
    assert!(
        text.contains("dialer: mode=refusing reason=no-confirmation"),
        "{text}"
    );
    // H-4: nothing counts the sends (the walk makes no account and the tool sends nothing), so the
    // report says so and never prints a 0 that looks measured.
    assert!(text.contains("sends: not measured"), "{text}");
    assert!(!text.contains("sends: 0"), "{text}");
    assert!(text.contains("confirmation: none written"), "{text}");
    // The rehearsal, confirmed: the app chose to dial, and still made no real backend.
    let confirmed = [dialer_line("telegram", "confirmed")];
    let r = pass(&good_walk(), &confirmed, true, RunKind::Rehearsal);
    assert!(r.pass, "{}", failures(&r));

    // THE MODE COMPARE (critic 16): the line must equal the order, in both kinds of run.
    assert_eq!(expected_dialer(true), ("telegram", "confirmed"));
    assert_eq!(expected_dialer(false), ("refusing", "no-confirmation"));
    let v = pass(&good_walk(), &refusing, true, RunKind::Rehearsal);
    expect_fail(&v, "does not match the order");
    let v = pass(&good_walk_for(false), &confirmed, false, RunKind::Rehearsal);
    expect_fail(&v, "does not match the order");
    let mut real_refusing = real_log();
    real_refusing[0] = dialer_line("refusing", "no-confirmation");
    let v = pass(&good_walk(), &real_refusing, true, RunKind::Real);
    expect_fail(&v, "does not match the order");
    let mut other_reason = real_log();
    other_reason[0] = dialer_line("telegram", "stale");
    let v = pass(&good_walk(), &other_reason, true, RunKind::Real);
    expect_fail(&v, "does not match the order");
    // No dialer line, or more than one: not a pass.
    let v = pass(&good_walk(), &real_log()[1..], true, RunKind::Real);
    expect_fail(&v, "no dialer line");
    let mut twice = real_log();
    twice.push(dialer_line("telegram", "confirmed"));
    expect_fail(
        &pass(&good_walk(), &twice, true, RunKind::Real),
        "more than once",
    );

    // The walk.
    let mut w = good_walk();
    w.steps.push(step("password", "input"));
    expect_fail(
        &pass(&w, &real_log(), true, RunKind::Real),
        "after the QR code",
    );
    // A `qr` step the app did not follow with its `login_qr` is no QR code and no refresh.
    let mut w = good_walk();
    w.steps.push(step("qr", "qr"));
    expect_fail(
        &pass(&w, &real_log(), true, RunKind::Real),
        "after the QR code",
    );
    // A refreshed code is a new link: another SHOWN `qr` after the first is no step of the login.
    let mut w = good_walk();
    w.steps.push(qr_step());
    assert!(
        pass(&w, &real_log(), true, RunKind::Real).pass,
        "a refreshed code is not a step after the QR code"
    );
    // ... but only that: the right word with another kind, or another word with the kind `qr`, is not.
    for (word, kind) in [("qr", "input"), ("phone", "qr")] {
        let mut w = good_walk();
        w.steps.push(Step {
            shown: true,
            ..step(word, kind)
        });
        expect_fail(
            &pass(&w, &real_log(), true, RunKind::Real),
            "after the QR code",
        );
    }
    let mut w = good_walk();
    w.steps = vec![];
    w.qr_after_ms = None;
    expect_fail(
        &pass(&w, &real_log(), true, RunKind::Real),
        "no QR code was shown",
    );
    // A `qr` step that was never shown (no `login_qr`) is no QR code either.
    let mut w = good_walk();
    w.steps = vec![step("qr", "qr")];
    expect_fail(
        &pass(&w, &real_log(), true, RunKind::Real),
        "no QR code was shown",
    );
    let mut w = good_walk();
    w.steps = vec![step("code", "code"), qr_step()];
    assert!(
        pass(&w, &real_log(), true, RunKind::Real).pass,
        "steps BEFORE the qr are not after it"
    );
    for (accounts, why) in [(Some(1), "has an account"), (None, "not measured")] {
        let mut w = good_walk();
        w.accounts = accounts;
        expect_fail(&pass(&w, &real_log(), true, RunKind::Real), why);
    }
    for (closed, why) in [
        (Some(false), "refused the page change"),
        (None, "not closed"),
    ] {
        let mut w = good_walk();
        w.closed = closed;
        expect_fail(&pass(&w, &real_log(), true, RunKind::Real), why);
    }
    for (exit, why) in [
        (ExitHow::Killed, "killed"),
        (ExitHow::NotSeen, "end was not seen"),
        (ExitHow::Quit(1), "failure status"),
    ] {
        let mut w = good_walk();
        w.exit = exit;
        expect_fail(&pass(&w, &real_log(), true, RunKind::Real), why);
    }

    // A REAL run: every missing line fails ("not measured" is never a pass there).
    let without = |gone: &str| -> Vec<String> {
        real_log()
            .into_iter()
            .filter(|l| !l.contains(gone))
            .collect()
    };
    expect_fail(
        &pass(&good_walk(), &without("gate counts"), true, RunKind::Real),
        "counts line is missing",
    );
    expect_fail(
        &pass(&good_walk(), &without("lock-down"), true, RunKind::Real),
        "lock-down word was not measured",
    );
    // Only a periodic line, no final one: not measured either.
    let periodic = [
        dialer_line("telegram", "confirmed"),
        lockdown_line("on"),
        counts_line("running", &[("dials", 2)]),
    ];
    expect_fail(
        &pass(&good_walk(), &periodic, true, RunKind::Real),
        "counts line is missing",
    );
    // A line with a number missing is no measurement.
    let mut unreadable = without("gate counts");
    unreadable.push(good_stop().replace("rule=0 ", ""));
    expect_fail(
        &pass(&good_walk(), &unreadable, true, RunKind::Real),
        "could not be read",
    );
    let lockdown = |word: &str| -> Vec<String> {
        let mut l = real_log();
        l[1] = lockdown_line(word);
        l
    };
    for word in ["unavailable", "other", "unknown"] {
        let v = pass(&good_walk(), &lockdown(word), true, RunKind::Real);
        expect_fail(&v, "lock-down is not on");
        assert!(v.findings.iter().any(|f| f.contains("not `on`")), "{word}");
    }
    let with_stop = |values: &[(&str, u64)]| -> Vec<String> {
        let mut l = without("when=\"stop\"");
        l.push(counts_line("stop", values));
        l
    };
    expect_fail(
        &pass(&good_walk(), &with_stop(&[]), true, RunKind::Real),
        "dialled nothing",
    );
    expect_fail(
        &pass(
            &good_walk(),
            &with_stop(&[("dials", 2), ("dial_failures", 2)]),
            true,
            RunKind::Real,
        ),
        "every dial the gate made failed",
    );
    assert!(
        pass(
            &good_walk(),
            &with_stop(&[("dials", 2), ("dial_failures", 1)]),
            true,
            RunKind::Real,
        )
        .pass,
        "one dial that failed is not every dial"
    );
    for (key, why) in [
        ("rule", "by its address list"),
        ("port", "by their port"),
        ("address_type", "by their address type"),
    ] {
        let v = pass(
            &good_walk(),
            &with_stop(&[("dials", 2), (key, 1)]),
            true,
            RunKind::Real,
        );
        expect_fail(&v, why);
    }

    // A REHEARSAL: the real backend never ran. A counts line, a lock-down line or a line about the
    // helper fails it.
    let v = pass(
        &good_walk_for(false),
        &[refusing[0].clone(), good_stop()],
        false,
        RunKind::Rehearsal,
    );
    expect_fail(&v, "gate counts line appeared in a rehearsal");
    let v = pass(
        &good_walk_for(false),
        &[refusing[0].clone(), lockdown_line("on")],
        false,
        RunKind::Rehearsal,
    );
    expect_fail(&v, "lock-down line appeared in a rehearsal");
    let hello =
        format!("{LEAD} kourier_telegram::supervisor: the TDLib helper reached its first state");
    let v = pass(
        &good_walk_for(false),
        &[refusing[0].clone(), hello],
        false,
        RunKind::Rehearsal,
    );
    expect_fail(&v, "TDLib helper appeared in a rehearsal");
    let hello = format!("{LEAD} kourier_telegram::actor: telegram helper said hello");
    let v = pass(
        &good_walk_for(false),
        &[refusing[0].clone(), hello],
        false,
        RunKind::Rehearsal,
    );
    expect_fail(&v, "TDLib helper appeared in a rehearsal");
}

/// The app's memory is read from the text of its status, in kB, and reported in MiB; a status without
/// the two lines is no measurement.
#[test]
fn the_apps_memory_is_read_from_its_status_and_reported_in_mib() {
    let status = "Name:\tkourier\nVmPeak:\t 900000 kB\nVmHWM:\t  262144 kB\nVmRSS:\t  204800 kB\nThreads:\t9\n";
    assert_eq!(parse_status_memory(status), Some((204_800, 262_144)));
    assert_eq!(parse_status_memory("VmRSS:\t 5 kB\n"), None);
    assert_eq!(
        parse_status_memory("VmRSS:\t many kB\nVmHWM:\t 1 kB\n"),
        None
    );
    assert_eq!(parse_status_memory(""), None);
    let walk = good_walk();
    let log = facts(&real_log());
    let v = verdict(&walk, &log, true, RunKind::Real);
    let text = lines(&Report {
        profile: "r0",
        env: "test",
        build: None,
        confirmed: true,
        kind: RunKind::Real,
        walk: &walk,
        log: &log,
        verdict: &v,
    })
    .join("\n");
    assert!(text.contains("memory 200 MiB (peak 256 MiB)"), "{text}");
    let none = Walk {
        memory_kb: None,
        ..good_walk()
    };
    let text = lines(&Report {
        profile: "r0",
        env: "test",
        build: None,
        confirmed: true,
        kind: RunKind::Real,
        walk: &none,
        log: &log,
        verdict: &v,
    })
    .join("\n");
    assert!(text.contains("memory not measured"), "{text}");
}

/// The walk's step words: a fixed vocabulary, anything else is `other`.
#[test]
fn a_step_is_named_by_a_fixed_word_or_not_at_all() {
    assert_eq!(step_word("qr"), "qr");
    assert_eq!(step_word("email-code-again"), "email-code-again");
    assert_eq!(step_word("+380501234567"), "unknown");
    assert_eq!(step_word(""), "unknown");
}

/// The step kinds the dev control names are a fixed vocabulary too: anything else is `other`.
#[test]
fn a_step_kind_is_named_by_a_fixed_word_or_other() {
    for kind in ["qr", "code", "emoji", "input", "progress", "done", "failed"] {
        assert_eq!(kind_word(kind), kind);
    }
    assert_eq!(kind_word("+380501234567"), "other");
    assert_eq!(kind_word(""), "other");
}

// ---- H-1: what the app SHOWED, not what a step is called -------------------------------------------

fn report_of(walk: &Walk, log: &LogFacts, confirmed: bool, kind: RunKind) -> String {
    let v = verdict(walk, log, confirmed, kind);
    lines(&Report {
        profile: "r0",
        env: "test",
        build: Some("a1b2c3d4e5"),
        confirmed,
        kind,
        walk,
        log,
        verdict: &v,
    })
    .join("\n")
}

/// H-1 (the review's F1, a BLOCKER): a login that FAILS at the step `qr` is not a QR code. A walk whose
/// only step is `(qr, failed)` fails, says where, and shows `first QR step: never`. Mutants: the reader
/// back to the step word for the QR (no `shown`, no kind); the shown requirement dropped; the failed
/// check dropped.
#[test]
fn a_login_that_failed_at_the_qr_step_is_no_qr_code() {
    let failed_at_qr = Walk {
        steps: vec![step("qr", "failed")],
        errors: 1,
        ..good_walk()
    };
    let v = pass(&failed_at_qr, &real_log(), true, RunKind::Real);
    expect_fail(&v, "the login failed at qr before the QR code");
    expect_fail(&v, "no QR code was shown");
    expect_fail(&v, "reported 1 login or account errors");
    let text = report_of(
        &Walk {
            qr_after_ms: None,
            ..failed_at_qr
        },
        &facts(&real_log()),
        true,
        RunKind::Real,
    );
    assert!(text.contains("connect: FAIL"), "{text}");
    assert!(text.contains("first QR step: never"), "{text}");
    assert!(
        text.contains("QR codes shown: 0; login errors: 1"),
        "{text}"
    );

    // A failure AFTER a shown code says so, and a refused step names its word from the known list.
    let mut w = good_walk();
    w.steps.push(step("phone", "failed"));
    let v = pass(&w, &real_log(), true, RunKind::Real);
    expect_fail(&v, "the login failed at phone after the QR code");
    assert!(
        !failures(&v).contains("a login step came after"),
        "a failed step has its own sentence: {}",
        failures(&v)
    );
    let mut w = good_walk();
    w.steps.push(step("+380501234567", "failed"));
    expect_fail(
        &pass(&w, &real_log(), true, RunKind::Real),
        "the login failed at unknown after",
    );

    // A refreshed code passes and is counted.
    let mut w = good_walk();
    w.steps.push(qr_step());
    let text = report_of(&w, &facts(&real_log()), true, RunKind::Real);
    assert!(text.contains("connect: PASS"), "{text}");
    assert!(text.contains("QR codes shown: 2;"), "{text}");
    assert!(text.contains("steps after it: 0;"), "{text}");

    // Errors of a login or an account fail the walk, even with a shown code and nothing else wrong.
    let w = Walk {
        errors: 2,
        ..good_walk()
    };
    expect_fail(
        &pass(&w, &real_log(), true, RunKind::Real),
        "the app reported 2 login or account errors",
    );

    // A link on the app's output fails: the tool started the app with the link switched off.
    let w = Walk {
        qr_link_printed: true,
        ..good_walk()
    };
    let v = pass(&w, &real_log(), true, RunKind::Real);
    expect_fail(&v, "the app printed the QR link");
    let text = report_of(&w, &facts(&real_log()), true, RunKind::Real);
    assert!(text.contains("QR link in the app's output: yes"), "{text}");
}

// ---- H-2: every gate session of the walk is judged -------------------------------------------------

/// H-2 (the review's F2, HIGH): the final line of EVERY gate session counts. Two stop lines, the first
/// with refusals and failed dials, the second clean: FAIL with the stale-list finding naming the sum and
/// the restart sentence. Mutants: `.rev().find()` back; the sums replaced by the last line's values; the
/// session count dropped.
#[test]
fn every_gate_session_of_the_walk_is_judged_and_a_restart_is_a_failure() {
    let mut log = real_log();
    log.pop();
    log.push(counts_line(
        "stop",
        &[
            ("accepted", 2),
            ("dials", 2),
            ("dial_failures", 2),
            ("rule", 2),
        ],
    ));
    log.push(good_stop());
    let v = pass(&good_walk(), &log, true, RunKind::Real);
    assert!(!v.pass);
    expect_fail(&v, "the helper was started 2 times during the walk");
    expect_fail(&v, "the gate refused connections by its address list");
    assert!(
        v.findings
            .iter()
            .any(|f| f.contains("refused 2 connection(s) by its address list: the list is stale")),
        "{:?}",
        v.findings
    );
    // The sums are over the final lines: 2 + 2 dials, 2 failed: not every dial failed; port and address
    // type sum the same way.
    assert!(
        !failures(&v).contains("every dial"),
        "dials are summed: {}",
        failures(&v)
    );
    let mut log = real_log();
    log.pop();
    log.push(counts_line(
        "stop",
        &[("dials", 1), ("dial_failures", 1), ("port", 1)],
    ));
    log.push(counts_line(
        "stop",
        &[("dials", 1), ("dial_failures", 1), ("address_type", 1)],
    ));
    let v = pass(&good_walk(), &log, true, RunKind::Real);
    expect_fail(&v, "every dial the gate made failed");
    expect_fail(&v, "by their port");
    expect_fail(&v, "by their address type");
    // Two stop lines with nothing refused still fail: a normal walk makes one gate session.
    let mut log = real_log();
    log.push(good_stop());
    expect_fail(
        &pass(&good_walk(), &log, true, RunKind::Real),
        "started 2 times",
    );

    // Every lock-down line must be `on`, not the first alone.
    let mut log = real_log();
    log.insert(2, lockdown_line("unavailable"));
    let v = pass(&good_walk(), &log, true, RunKind::Real);
    expect_fail(&v, "lock-down is not on");
    assert!(v.findings.iter().any(|f| f.contains("not `on`")));
    let text = report_of(&good_walk(), &facts(&log), true, RunKind::Real);
    assert!(text.contains("lock-down: on, unavailable"), "{text}");

    // The lock-down line is told once per account actor: one line and two final lines is a restart
    // inside one actor (judged above); two lock-down lines and one final line is an account actor whose
    // gate never said its end (R-2: the claim is about actors, not about helper starts).
    let mut log = real_log();
    log.insert(2, lockdown_line("on"));
    expect_fail(
        &pass(&good_walk(), &log, true, RunKind::Real),
        "an account actor told its lock-down word without a final counts line of its gate",
    );

    // One session: a periodic line is a partial count of the same gate, judged by the stop line after it.
    let one = [
        dialer_line("telegram", "confirmed"),
        lockdown_line("on"),
        counts_line(
            "running",
            &[("accepted", 1), ("dials", 1), ("dial_failures", 1)],
        ),
        counts_line(
            "stop",
            &[("accepted", 3), ("dials", 2), ("dial_failures", 1)],
        ),
    ];
    let v = pass(&good_walk(), &one, true, RunKind::Real);
    assert!(v.pass, "{}", failures(&v));
    // ... and never summed with it: the running line alone, with refusals the stop line does not have,
    // changes nothing (counts are cumulative, the stop line holds what the periodic one said).
    let periodic_refusal = [
        dialer_line("telegram", "confirmed"),
        lockdown_line("on"),
        counts_line("running", &[("dials", 1), ("rule", 5)]),
        good_stop(),
    ];
    assert!(pass(&good_walk(), &periodic_refusal, true, RunKind::Real).pass);
}

// ---- H-4: sends are not measured ------------------------------------------------------------------

/// H-4 (the review's F4): no report prints `sends: 0`. The walk makes no account and the tool sends
/// nothing, but nothing counts it, so the words are `sends: not measured`. Mutant: the constant back.
#[test]
fn no_report_prints_a_sends_count_it_did_not_measure() {
    for (walk, log, confirmed, kind) in [
        (good_walk(), facts(&real_log()), true, RunKind::Real),
        (
            good_walk_for(false),
            facts(&[dialer_line("refusing", "no-confirmation")]),
            false,
            RunKind::Rehearsal,
        ),
        (Walk::default(), LogFacts::default(), true, RunKind::Real),
    ] {
        let text = report_of(&walk, &log, confirmed, kind);
        assert!(text.contains("  sends: not measured"), "{text}");
        assert!(!text.contains("sends: 0"), "{text}");
    }
}

// ---- H-5: who took the confirmation ---------------------------------------------------------------

/// H-5 (the review's F5): the walk looked at the confirmation file before its own removal. A confirmed
/// order whose file still stood FAILS; an unconfirmed order with a file standing FAILS; a walk that did
/// not look is not a PASS. Mutant: the look moved after the removal (in `connect.rs`' tests).
#[test]
fn the_verdict_knows_who_took_the_confirmation() {
    let with = |conf| Walk {
        confirmation: conf,
        ..good_walk()
    };
    let refusing = [dialer_line("refusing", "no-confirmation")];
    // A confirmed order: only `taken by the app` passes.
    for conf in [
        Some(Confirmation::Left),
        Some(Confirmation::NoneWritten),
        None,
    ] {
        let v = pass(&with(conf), &real_log(), true, RunKind::Real);
        assert!(!v.pass);
    }
    expect_fail(
        &pass(
            &with(Some(Confirmation::Left)),
            &real_log(),
            true,
            RunKind::Real,
        ),
        "the app did not take its confirmation",
    );
    expect_fail(
        &pass(&with(None), &real_log(), true, RunKind::Real),
        "was not observed",
    );
    // An unconfirmed order: only `none written` passes; a file standing means something wrote one.
    assert!(
        pass(
            &with(Some(Confirmation::NoneWritten)),
            &refusing,
            false,
            RunKind::Rehearsal
        )
        .pass
    );
    for conf in [Some(Confirmation::Left), Some(Confirmation::TakenByApp)] {
        expect_fail(
            &pass(&with(conf), &refusing, false, RunKind::Rehearsal),
            "stood although the order was not confirmed",
        );
    }
    // The words of the report.
    for (conf, words) in [
        (Confirmation::TakenByApp, "confirmation: taken by the app"),
        (
            Confirmation::Left,
            "confirmation: left (removed by the tool)",
        ),
        (Confirmation::NoneWritten, "confirmation: none written"),
    ] {
        let text = report_of(&with(Some(conf)), &facts(&real_log()), true, RunKind::Real);
        assert!(text.contains(words), "{text}");
    }
    let text = report_of(&with(None), &facts(&real_log()), true, RunKind::Real);
    assert!(text.contains("confirmation: not observed"), "{text}");
}

/// H-9: a rehearsal's report carries the count of the walk's needles in the app's raw stderr; a real
/// run, which keeps no needle list, prints no such line.
#[test]
fn a_rehearsal_reports_the_needles_in_the_raw_stderr_and_a_real_run_does_not() {
    let refusing = facts(&[dialer_line("refusing", "no-confirmation")]);
    let walk = Walk {
        raw_needles: Some(0),
        ..good_walk_for(false)
    };
    let text = report_of(&walk, &refusing, false, RunKind::Rehearsal);
    assert!(
        text.contains("  raw needles in the app's stderr: 0"),
        "{text}"
    );
    let walk = Walk {
        raw_needles: Some(3),
        ..good_walk_for(false)
    };
    let text = report_of(&walk, &refusing, false, RunKind::Rehearsal);
    assert!(
        text.contains("  raw needles in the app's stderr: 3"),
        "{text}"
    );
    let text = report_of(&good_walk(), &facts(&real_log()), true, RunKind::Real);
    assert!(!text.contains("raw needles"), "{text}");
}
