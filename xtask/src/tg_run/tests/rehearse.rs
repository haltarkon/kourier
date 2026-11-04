//! The rehearsal (`tg-run rehearse`): its places (critic 4), the copies of the fake's constants (X26),
//! the leak search, the judges of what a walk printed, and the pieces of the tool's commands. The
//! rehearsal itself (the app in `unshare -rn`) is run by `cargo xtask tg-run rehearse`, not here.

use std::ffi::OsStr;
use std::io::Read as _;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use super::report::qr_step;
use super::{Scratch, args, error_of};
use crate::root;
use crate::tg_run::connect::{
    ORDER_CONFIRMED_LINE, ORDER_UNCONFIRMED_LINE, REHEARSAL_RANDOM_ENV, fresh_nonce,
    make_non_dumpable, random_source,
};
use crate::tg_run::preflight::TEST_ENV;
use crate::tg_run::profile::Where;
use crate::tg_run::profile::{ProfileDir, State};
use crate::tg_run::rehearse::{
    APP_PATH_MARGIN, FAKE_CONSTANTS, Order, SOCKET_PATH_MAX, Tool, WALK_DATA, WALK_PROFILE,
    WIPE_DATA, WIPE_PROFILE, fits_a_socket, hits, holds, in_unshare, judge_walk, leaks_in,
    make_random_source, needles, place_under, raw_needles, registered_in,
};
use crate::tg_run::report::{
    Confirmation, ExitHow, LogFacts, Report, RunKind, Walk, lines, verdict,
};

// ---- The places (critic 4) -----------------------------------------------------------------------

/// The longest checkout path (bytes) the rehearsal fits a socket path from: the places below its
/// `target/` take 41 bytes more, and the tool keeps 20 for the app's own socket. A longer checkout is
/// refused when the rehearsal starts (`rehearsal`), so the test judges this length, not the path of
/// the checkout it happens to run in.
const LONGEST_CHECKOUT: usize = 46;

/// A `target/` directory below a checkout path of `len` bytes.
fn target_of_a_checkout(len: usize) -> PathBuf {
    PathBuf::from("/".to_owned() + &"c".repeat(len - 1)).join("target")
}

/// Critic 4: the rehearsal's places are short. A place below the target directory of a checkout of
/// the longest supported path, with the longest seconds a clock will show for centuries and the room
/// the tool keeps for a socket of the app's, fits a Unix socket path (107 bytes); one byte more does
/// not; the tool's own places are the shape the profile rules accept. Mutant: the margin's check
/// inverted (the negative sample fails).
#[test]
fn the_rehearsals_places_fit_a_socket_path_and_are_the_shape_the_rules_accept() {
    let target = target_of_a_checkout(LONGEST_CHECKOUT);
    for secs in [1_793_500_000_u64, 9_999_999_999] {
        for (data, name) in [(WALK_DATA, WALK_PROFILE), (WIPE_DATA, WIPE_PROFILE)] {
            let place = place_under(&target, secs, data, name);
            let len = place.as_os_str().len();
            assert!(
                fits_a_socket(&place),
                "the place is {len} bytes: with {APP_PATH_MARGIN} for the app's own socket it passes {SOCKET_PATH_MAX}"
            );
            assert!(len + APP_PATH_MARGIN <= SOCKET_PATH_MAX);
            let resolved = Where {
                profile: None,
                rehearsal_place: Some(place.clone()),
            }
            .resolve(&|_| None, &target)
            .expect("the rule accepts the place");
            assert!(resolved.rehearsal);
            assert_eq!(resolved.dir, place);
        }
    }
    // A checkout one byte longer no longer fits.
    let longer = place_under(
        &target_of_a_checkout(LONGEST_CHECKOUT + 1),
        9_999_999_999,
        WALK_DATA,
        WALK_PROFILE,
    );
    assert!(!fits_a_socket(&longer));
    // The check does refuse a long place.
    let long = PathBuf::from("/".to_owned() + &"a".repeat(SOCKET_PATH_MAX - APP_PATH_MARGIN));
    assert!(!fits_a_socket(&long));
    let fits = PathBuf::from("/".to_owned() + &"a".repeat(SOCKET_PATH_MAX - APP_PATH_MARGIN - 1));
    assert!(fits_a_socket(&fits));
}

// ---- X26 ------------------------------------------------------------------------------------------

/// X26: the tool's constants are the fake's, every one of them, and every one is searched. The file is
/// read: a constant added to the fake and not copied, a value changed there, or one dropped from the
/// search fails. The fake's own discipline holds for the copies: no value inside another (but the right
/// tail ends the own number), none holds the sentinel, all distinct. Mutants: a value of the table
/// changed; one constant dropped from the needles.
#[test]
fn every_rehearsal_constant_is_searched_and_matches_the_fake() {
    let path = root().join("crates/kourier-fake/src/telegram.rs");
    let text = std::fs::read_to_string(path).expect("the fake's source");
    let found: Vec<(String, String)> = text
        .lines()
        .filter_map(|l| {
            let rest = l.strip_prefix("pub const ")?;
            let (name, rest) = rest.split_once(": &str = \"")?;
            Some((name.to_owned(), rest.strip_suffix("\";")?.to_owned()))
        })
        .collect();
    let copies: Vec<(String, String)> = FAKE_CONSTANTS
        .iter()
        .map(|(n, v)| ((*n).to_owned(), (*v).to_owned()))
        .collect();
    assert_eq!(
        found, copies,
        "the tool's copies of the fake's constants (names, values, the file's order)"
    );
    // Each constant, alone in a text, is found under its own label; nothing is found in a clean text.
    let needles = needles("nonce", "hash");
    for (name, value) in FAKE_CONSTANTS {
        let labels = hits(format!("x {value} y").as_bytes(), &needles);
        assert!(
            labels.contains(&format!("the fake's {name}")),
            "{name} is not searched: {labels:?}"
        );
    }
    assert!(hits(b"nothing of the fake stands here", &needles).is_empty());
    // The fake's own substring discipline.
    for (i, (name, value)) in FAKE_CONSTANTS.iter().enumerate() {
        assert!(!value.is_empty(), "{name}");
        assert!(
            !value.contains(crate::tg_run::stage::SENTINEL_TAIL),
            "{name}"
        );
        for (j, (other, text)) in FAKE_CONSTANTS.iter().enumerate() {
            if i == j || (*name, *other) == ("RIGHT_TAIL", "OWN_NUMBER") {
                continue;
            }
            assert!(!text.contains(value), "{name} is inside {other}");
        }
    }
    let mut values: Vec<&str> = FAKE_CONSTANTS.iter().map(|(_, v)| *v).collect();
    values.sort_unstable();
    values.dedup();
    assert_eq!(values.len(), FAKE_CONSTANTS.len());
}

// ---- The leak search -------------------------------------------------------------------------------

/// The search finds every needle in any file of a place, in a nested directory too, says WHICH needle
/// (by label) and where (the relative path) and never the value; it does not read the stamped program
/// (`tg-run/bin`), which holds the fake's names as code; a clean place has none. Mutants: the stamped
/// program read; the hash of the nonce left out of the needles.
#[test]
fn a_leak_is_found_in_any_file_of_the_place_and_the_stamped_program_is_not_read() {
    let scratch = Scratch::new("leaks");
    let nonce = "ab".repeat(32);
    let hash = "cd".repeat(32);
    let needles = needles(&nonce, &hash);
    assert_eq!(needles.len(), FAKE_CONSTANTS.len() + 3);
    let place = scratch.0.join("place");
    std::fs::create_dir_all(place.join("tg-run/bin")).expect("dirs");
    std::fs::create_dir_all(place.join("kourier/data/deep")).expect("dirs");
    std::fs::create_dir_all(place.join("kourier/bin")).expect("dirs");
    assert!(
        leaks_in(&place, &needles).is_empty(),
        "an empty place has no leak"
    );
    let mut values: Vec<String> = FAKE_CONSTANTS
        .iter()
        .map(|(_, v)| (*v).to_owned())
        .collect();
    values.extend([crate::tg_run::stage::SENTINEL_TAIL.to_owned(), nonce, hash]);
    for (i, value) in values.iter().enumerate() {
        let dir = if i % 2 == 0 {
            "kourier/data/deep"
        } else {
            "tg-run"
        };
        std::fs::write(
            place.join(dir).join(format!("f{i}")),
            format!("binary\0\u{1}before {value} after"),
        )
        .expect("a file");
    }
    // The stamped program holds all of them and is not reported; another directory named `bin` is.
    std::fs::write(place.join("tg-run/bin/kourier"), values.join(" ")).expect("a file");
    std::fs::write(place.join("kourier/bin/other"), &values[0]).expect("a file");
    let found = leaks_in(&place, &needles);
    // One finding per value, the right tail inside the own number a second time, and `kourier/bin`'s.
    assert_eq!(found.len(), values.len() + 2, "{found:?}");
    for (file, label) in &found {
        assert!(!file.starts_with("tg-run/bin"), "{file}");
        for value in &values {
            assert!(
                !file.contains(value.as_str()) && !label.contains(value.as_str()),
                "a value reached a finding: {file} {label}"
            );
        }
    }
    for (name, _) in FAKE_CONSTANTS {
        assert!(
            found
                .iter()
                .any(|(_, label)| label == &format!("the fake's {name}")),
            "{name} not found"
        );
    }
    for label in [
        "the sentinel tail",
        "the confirmation's nonce",
        "the hash of the confirmation's nonce",
    ] {
        assert!(found.iter().any(|(_, l)| l == label), "{label}");
    }
}

/// A short code counts as a whole number only: a log line's timestamp has six digits of microseconds,
/// which spell a five-digit code now and then, and the rehearsal must not fail on that. Every other needle
/// counts anywhere, the sentinel tail (the tail of a number) among them. Mutant: the whole-number rule
/// dropped.
#[test]
fn a_short_code_counts_only_as_a_whole_number_and_every_other_needle_anywhere() {
    let code = FAKE_CONSTANTS
        .iter()
        .find(|(name, _)| *name == "CODE")
        .map(|(_, v)| *v)
        .expect("the code");
    assert_eq!(code.len(), 5);
    let stamp = format!("2026-11-01T10:00:00.{code}0Z  INFO a line");
    assert!(
        !holds(&stamp, code),
        "a timestamp's microseconds are no code"
    );
    assert!(!holds(&format!("9{code}"), code) && !holds(&format!("{code}9"), code));
    for text in [
        format!("code={code}"),
        code.to_owned(),
        format!("{code} x"),
        // A hit further on, after a near miss.
        format!("1{code} then {code}."),
    ] {
        assert!(holds(&text, code), "{text}");
    }
    // A long number, a name, the sentinel tail and a hash count anywhere.
    let own = FAKE_CONSTANTS
        .iter()
        .find(|(name, _)| *name == "OWN_TG_ID")
        .map(|(_, v)| *v)
        .expect("the id");
    assert!(holds(&format!("tg:{own}9"), own) && holds(&format!("1{own}"), own));
    assert!(holds("name Rehearsal Tgowner here", FAKE_CONSTANTS[0].1));
    assert!(holds(
        &format!("+38{}", crate::tg_run::stage::SENTINEL_TAIL),
        crate::tg_run::stage::SENTINEL_TAIL
    ));
    assert!(!holds("anything", ""));
    // The same rule in the leak search.
    let found = hits(stamp.as_bytes(), &needles("", ""));
    assert!(found.is_empty(), "{found:?}");
}

/// The needles a rehearsal's walk searches the app's raw stderr for: the fake's constants and the
/// sentinel tail always, the nonce and its hash only for a walk that made one; none of them empty.
#[test]
fn the_raw_stderr_needles_are_the_leak_searchs_own_and_the_nonce_only_when_there_is_one() {
    let nonce = "ab".repeat(32);
    let with = raw_needles(Some(&nonce)).expect("needles");
    let without = raw_needles(None).expect("needles");
    assert_eq!(with.len(), without.len() + 2);
    assert!(with.contains(&nonce) && !without.contains(&nonce));
    assert!(with.iter().all(|n| !n.is_empty()) && without.iter().all(|n| !n.is_empty()));
    for (_, value) in FAKE_CONSTANTS {
        assert!(without.contains(&value.to_owned()), "{value}");
    }
    assert!(without.contains(&crate::tg_run::stage::SENTINEL_TAIL.to_owned()));
}

// ---- What a walk printed -----------------------------------------------------------------------------

/// The text a rehearsal walk prints, made by the tool's own report writer (so a change to the report
/// shows here): the lines of `report::lines` for a passing rehearsal and the lines `connect` prints
/// before it.
fn printed(order: Order, edit: impl FnOnce(&mut Walk, &mut LogFacts)) -> String {
    let confirmed = order == Order::Confirmed;
    let mut walk = Walk {
        steps: vec![qr_step()],
        qr_after_ms: Some(2_345),
        closed: Some(true),
        accounts: Some(0),
        memory_kb: Some((150 * 1024, 170 * 1024)),
        exit: ExitHow::Quit(0),
        confirmation: Some(if confirmed {
            Confirmation::TakenByApp
        } else {
            Confirmation::NoneWritten
        }),
        raw_needles: Some(0),
        ..Walk::default()
    };
    let (mode, reason) = crate::tg_run::report::expected_dialer(confirmed);
    let mut log = LogFacts {
        dialer: vec![(mode.to_owned(), reason.to_owned())],
        ..LogFacts::default()
    };
    edit(&mut walk, &mut log);
    let verdict = verdict(&walk, &log, confirmed, RunKind::Rehearsal);
    let report = Report {
        profile: "p (rehearsal)",
        env: "test",
        build: None,
        confirmed,
        kind: RunKind::Rehearsal,
        walk: &walk,
        log: &log,
        verdict: &verdict,
    };
    let start = if confirmed {
        "start:    kourier --dev-control --size=1200x800 (variables set by the tool: KOURIER_DEV_TELEGRAM_DIAL KOURIER_DEV_TELEGRAM_ENV KOURIER_HOME)"
    } else {
        "start:    kourier --dev-control --size=1200x800 (variables set by the tool: KOURIER_DEV_TELEGRAM_ENV KOURIER_HOME)"
    };
    // The preflight's line as the tool writes it for a rehearsal (H-8): it looked at no disk.
    let cleared = crate::tg_run::preflight::Cleared {
        dir: ProfileDir {
            dir: PathBuf::from("/p"),
            rehearsal: true,
        },
        state: State::new("test"),
        warnings: Vec::new(),
        encrypted: None,
        head: None,
        ranges_verified: None,
        library: None,
    };
    let preflight = crate::tg_run::summary_text(
        &args(&["connect", "--env", "test", "--no-build"]).expect("args"),
        &cleared,
    );
    format!(
        "{preflight}\nprobe:    the staged app registers Telegram\n{}\n{start}\n{}\n",
        if confirmed {
            ORDER_CONFIRMED_LINE
        } else {
            ORDER_UNCONFIRMED_LINE
        },
        lines(&report).join("\n")
    )
}

/// The judge of a walk takes the walks the tool prints and refuses each way one can go wrong: the mode
/// compare (a confirmed order that the app refused, an unconfirmed one it dialled), the dial variable
/// named where it must not be and missing where it must, a nonce-looking value in the launch line, a
/// FAIL, a counts or lock-down line in a rehearsal, an account, a step after the QR, an app that did not
/// quit, a missing probe, a missing order line. Mutants: the dialer compare dropped; the check of the
/// dial variable's name dropped.
#[test]
fn the_judge_of_a_walk_takes_the_walks_it_should_and_refuses_the_rest() {
    for order in [Order::Unconfirmed, Order::Confirmed] {
        let good = printed(order, |_, _| {});
        let walked = judge_walk(&good, order).unwrap_or_else(|e| panic!("{order:?}: {e:#}"));
        assert_eq!(walked.qr_after_ms, 2_345);
        assert_eq!(walked.memory.as_deref(), Some("150 MiB (peak 170 MiB)"));
    }
    // The other order's walk is refused by each.
    let confirmed = printed(Order::Confirmed, |_, _| {});
    let unconfirmed = printed(Order::Unconfirmed, |_, _| {});
    assert!(judge_walk(&confirmed, Order::Unconfirmed).is_err());
    assert!(judge_walk(&unconfirmed, Order::Confirmed).is_err());
    // A confirmed order the app refused / an unconfirmed one the app dialled: the verdict itself fails
    // (X23), and the judge refuses the dialer line besides, with the PASS word put back.
    let refused = printed(Order::Confirmed, |_, log| {
        log.dialer = vec![("refusing".to_owned(), "no-confirmation".to_owned())];
    });
    assert!(judge_walk(&refused, Order::Confirmed).is_err());
    let forged = refused.replace("connect: FAIL", "connect: PASS");
    let error = error_of(judge_walk(&forged, Order::Confirmed));
    assert!(
        error.contains("dialer line") || error.contains("failure"),
        "{error}"
    );
    let dialled = printed(Order::Unconfirmed, |_, log| {
        log.dialer = vec![("telegram".to_owned(), "confirmed".to_owned())];
    })
    .replace("connect: FAIL", "connect: PASS")
    .replace("  FAILED: the dialer line does not match the order\n", "");
    let error = error_of(judge_walk(&dialled, Order::Unconfirmed));
    assert!(error.contains("dialer line"), "{error}");
    // The text edits the verdict does not see.
    let mistakes: [(&str, Order, &str, &str); 19] = [
        (
            "the dial variable named for an unconfirmed order",
            Order::Unconfirmed,
            "KOURIER_DEV_TELEGRAM_ENV KOURIER_HOME)",
            "KOURIER_DEV_TELEGRAM_DIAL KOURIER_HOME)",
        ),
        (
            "the dial variable missing for a confirmed order",
            Order::Confirmed,
            "KOURIER_DEV_TELEGRAM_DIAL ",
            "",
        ),
        (
            "a nonce-like value in the launch line",
            Order::Confirmed,
            "KOURIER_HOME)",
            "KOURIER_HOME) 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        ),
        (
            "an account after the close",
            Order::Unconfirmed,
            "accounts after the close: 0",
            "accounts after the close: 1",
        ),
        (
            "a step after the QR step",
            Order::Unconfirmed,
            "steps after it: 0",
            "steps after it: 1",
        ),
        (
            "the page change not answered",
            Order::Unconfirmed,
            "the page change: answered ok",
            "the page change: refused",
        ),
        (
            "the app killed",
            Order::Unconfirmed,
            "quit by itself (status 0)",
            "killed after its bound",
        ),
        (
            "a failure status",
            Order::Unconfirmed,
            "quit by itself (status 0)",
            "quit by itself (status 1)",
        ),
        (
            "the probe line missing",
            Order::Unconfirmed,
            "probe:    the staged app registers Telegram",
            "probe:    nothing",
        ),
        (
            "the order line of the other order",
            Order::Unconfirmed,
            ORDER_UNCONFIRMED_LINE,
            ORDER_CONFIRMED_LINE,
        ),
        (
            "a counts line in a rehearsal",
            Order::Unconfirmed,
            "  gate counts: not measured (rehearsal)",
            "  gate counts (stop): accepted=1",
        ),
        (
            "a lock-down word in a rehearsal",
            Order::Unconfirmed,
            "lock-down: not measured (rehearsal)",
            "lock-down: on",
        ),
        // H-8: a rehearsal looks at no disk and says so.
        (
            "a disk claimed in a rehearsal",
            Order::Unconfirmed,
            "; disk not checked;",
            "; disk not encrypted;",
        ),
        // H-5: the walk's own word on who took the confirmation.
        (
            "a confirmation left standing for a confirmed order",
            Order::Confirmed,
            "confirmation: taken by the app",
            "confirmation: left (removed by the tool)",
        ),
        (
            "a confirmation taken for an unconfirmed order",
            Order::Unconfirmed,
            "confirmation: none written",
            "confirmation: taken by the app",
        ),
        (
            "no word on the confirmation",
            Order::Confirmed,
            "  confirmation: taken by the app\n",
            "",
        ),
        // H-4: sends are not measured.
        (
            "a sends count nobody measured",
            Order::Unconfirmed,
            "sends: not measured",
            "sends: 0",
        ),
        // H-9: the walk's needles in the app's raw stderr.
        (
            "a needle in the app's raw stderr",
            Order::Confirmed,
            "raw needles in the app's stderr: 0",
            "raw needles in the app's stderr: 2",
        ),
        (
            "no count of the needles in the app's raw stderr",
            Order::Unconfirmed,
            "  raw needles in the app's stderr: 0\n",
            "",
        ),
    ];
    for (what, order, from, to) in mistakes {
        let text = printed(order, |_, _| {});
        assert!(
            text.contains(from),
            "{what}: the sample does not hold {from:?}"
        );
        let edited = text.replace(from, to);
        assert!(
            judge_walk(&edited, order).is_err(),
            "the judge took a walk with {what}"
        );
    }
    // And the verdict's own failures surface: a walk with an account says FAILED.
    let account = printed(Order::Unconfirmed, |walk, _| walk.accounts = Some(1));
    assert!(judge_walk(&account, Order::Unconfirmed).is_err());
}

// ---- The probe by itself ---------------------------------------------------------------------------------

#[test]
fn the_registered_kinds_are_read_from_the_networks_line() {
    assert_eq!(
        registered_in("networks registered=Telegram off=Signal on=Telegram\n"),
        Some(vec!["Telegram".to_owned()])
    );
    assert_eq!(
        registered_in("noise\nnetworks registered=Signal,Telegram off= on=\n"),
        Some(vec!["Signal".to_owned(), "Telegram".to_owned()])
    );
    assert_eq!(
        registered_in("networks registered= off=Telegram on=\n"),
        Some(Vec::new())
    );
    assert_eq!(registered_in("nothing\n"), None);
}

/// The probe the rehearsal runs by itself is the tool's own probe command inside `unshare -rn`: the
/// program, arguments, set variables and REMOVED variables are all kept.
#[test]
fn a_command_in_unshare_keeps_its_arguments_and_its_environment() {
    let mut cmd = Command::new("/staged/kourier");
    cmd.arg("--print-networks")
        .env("KOURIER_HOME", "/home/p")
        .env_remove("KOURIER_FAILPOINT");
    let wrapped = in_unshare(&cmd);
    assert_eq!(wrapped.get_program(), OsStr::new("unshare"));
    let args: Vec<&OsStr> = wrapped.get_args().collect();
    assert_eq!(
        args,
        [
            OsStr::new("-rn"),
            OsStr::new("/staged/kourier"),
            OsStr::new("--print-networks")
        ]
    );
    let envs: Vec<_> = wrapped.get_envs().collect();
    assert!(envs.contains(&(OsStr::new("KOURIER_HOME"), Some(OsStr::new("/home/p")))));
    assert!(envs.contains(&(OsStr::new("KOURIER_FAILPOINT"), None)));
}

// ---- The tool's commands -----------------------------------------------------------------------------------

fn tool() -> Tool {
    Tool {
        exe: PathBuf::from("/xtask/bin/xtask"),
        transcript: String::new(),
    }
}

/// Every command of the rehearsal is `unshare -rn <xtask> tg-run <args>` with the place named by
/// `--rehearsal-place`; a command on a kept root has no place, sets the scratch data home and removes
/// every variable that says "this is a test". Mutant: the namespace dropped.
#[test]
fn every_command_of_the_rehearsal_runs_in_its_own_namespace() {
    let tool = tool();
    let place = PathBuf::from("/t/tgr/1/x/kourier-tg-run/p");
    let cmd = tool.command(&["connect", "--env", "test"], Some(&place));
    assert_eq!(cmd.get_program(), OsStr::new("unshare"));
    let args: Vec<String> = cmd
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        args,
        [
            "-rn",
            "/xtask/bin/xtask",
            "tg-run",
            "connect",
            "--env",
            "test",
            "--rehearsal-place",
            "/t/tgr/1/x/kourier-tg-run/p"
        ]
    );
    let kept = tool.command(&["connect", "--env", "test"], None);
    let kept_args: Vec<String> = kept
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert_eq!(kept_args[..2], ["-rn", "/xtask/bin/xtask"]);
    assert!(!kept_args.iter().any(|a| a == "--rehearsal-place"));
}

/// The command runner captures both streams and the exit status, keeps the transcript for the leak
/// search, and kills a child at its bound (no wall time is asserted: a child that would run for half a
/// minute is given 300 ms and must not come back ok).
#[test]
fn the_runner_captures_both_streams_and_ends_a_child_at_its_bound() {
    let mut tool = tool();
    let mut cmd = Command::new("sh");
    cmd.args(["-c", "echo one; echo two >&2; exit 3"]);
    let (code, printed) = tool
        .capture(cmd, "a child", Duration::from_secs(60))
        .expect("the child ends");
    assert_eq!(code, 3);
    assert!(
        printed.contains("one\n") && printed.contains("two\n"),
        "{printed:?}"
    );
    assert!(tool.transcript.contains("one") && tool.transcript.contains("two"));
    let mut hang = Command::new("sh");
    hang.args(["-c", "exec sleep 30"]);
    let error = error_of(tool.capture(hang, "a hung child", Duration::from_millis(300)));
    assert!(error.contains("did not end in time"), "{error}");
}

/// A command on a kept root clears every variable that says "this is a test run" (so the go's refusal
/// is the one that comes, not the test check's), sets the scratch data home and names no place.
/// Mutant: one variable left in.
#[test]
fn a_command_on_a_kept_root_carries_no_test_variable() {
    let tool = tool();
    let xdg = PathBuf::from("/t/tgr/1/k");
    let cmd = tool.kept_command(&["connect", "--env", "test", "--profile", "p"], &xdg);
    let envs: Vec<_> = cmd.get_envs().collect();
    for name in TEST_ENV {
        assert!(
            envs.contains(&(OsStr::new(name), None)),
            "{name} is not removed"
        );
    }
    assert!(envs.contains(&(OsStr::new("XDG_DATA_HOME"), Some(xdg.as_os_str()))));
    assert!(
        !cmd.get_args().any(|a| a == "--rehearsal-place"),
        "a kept-root command names no place"
    );
}

// ---- The random source ---------------------------------------------------------------------------------------

/// A rehearsal place may name the file that stands for `/dev/urandom` (the rehearsal then knows the
/// nonce); a real run never looks at the variable. The rehearsal's parent derives the same 64 digits
/// from the bytes as the tool does. Mutant: the real run honours the variable.
#[test]
fn the_random_source_of_a_rehearsal_is_named_and_a_real_run_ignores_the_name() {
    let scratch = Scratch::new("random");
    let file = scratch.0.join("random-source");
    let nonce = make_random_source(&file).expect("the source");
    assert_eq!(nonce.len(), 64);
    let bytes = std::fs::read(&file).expect("bytes");
    assert_eq!(bytes.len(), 32);
    let path = file.to_string_lossy().into_owned();
    let var = move |k: &str| (k == REHEARSAL_RANDOM_ENV).then(|| path.clone());
    // The rehearsal reads the file: the tool's nonce is the parent's.
    let mut source = random_source(true, &var).expect("the source");
    let made =
        fresh_nonce(make_non_dumpable().expect("non-dumpable"), &mut *source).expect("nonce");
    assert_eq!(made, nonce);
    // A real run does not.
    let mut source = random_source(false, &var).expect("urandom");
    let mut other = [0u8; 32];
    source.read_exact(&mut other).expect("bytes");
    assert_ne!(
        other.as_slice(),
        bytes.as_slice(),
        "a real run read the named file"
    );
    // A rehearsal with no name reads the system's source.
    let mut source = random_source(true, &|_| None).expect("urandom");
    source.read_exact(&mut other).expect("bytes");
    assert_ne!(other.as_slice(), bytes.as_slice());
    // The file is private and is not made twice.
    assert_eq!(super::mode_of(&file), 0o600);
    assert!(make_random_source(&file).is_err());
}

// ---- The command line ----------------------------------------------------------------------------------------

#[test]
fn rehearse_takes_keep_and_no_build_and_nothing_secret() {
    let a = args(&["rehearse"]).expect("parses");
    assert!(!a.keep && !a.no_build);
    let a = args(&["rehearse", "--keep", "--no-build"]).expect("parses");
    assert!(a.keep && a.no_build);
    assert!(args(&["rehearse", "--keep-the-code"]).is_err());
}
