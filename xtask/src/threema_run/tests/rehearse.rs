//! The rehearsal's pieces (T9): the constants it copies from the fake, the needles and the search, the
//! reading of what the commands print, the terminal it types on, the cues and the plan, the list
//! watch's judge. The walk itself (a real app, the fake, `unshare -rn`) is the ONE real run of the
//! chunk, not a unit test. Nothing here starts the app or contacts a host; every wait is on an outcome.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use serde_json::json;

use super::{Scratch, digest_shape, id_shape};
use crate::threema_run::launch::rehearsal_socket_dir;
use crate::threema_run::profile::Where;
use crate::threema_run::prompt::{
    PROMPT_FIRST, digest_of_id, foreground_of_a_terminal, list_text, valid_id,
};
use crate::threema_run::rehearse::{
    Cue, FAKE_CONSTANTS, NOT_SHOWN, Plan, Sighting, constant, hits, in_unshare, judge_sighting,
    kinds_of, leaks_in, line_with_for_tests, needles, place_under, redaction_markers,
    run_on_a_terminal, verdict_of,
};
use crate::threema_run::steps::StepTiming;
use crate::threema_run::supervise::{REHEARSAL_SEND_TEXT, rehearsal_cue_ok};

// ---- The fake's constants ------------------------------------------------------------------------------

/// The `pub const NAME: &str = "VALUE";` lines of the fake's file, in its order.
fn fakes_constants() -> Vec<(String, String)> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("crates")
        .join("kourier-fake")
        .join("src")
        .join("threema.rs");
    let source = std::fs::read_to_string(&path).expect("the fake's source");
    source
        .lines()
        .filter_map(|l| l.strip_prefix("pub const "))
        .filter(|l| l.contains(": &str ="))
        .map(|l| {
            let (name, rest) = l.split_once(':').expect("a name");
            let value = rest.split('"').nth(1).expect("a quoted value").to_owned();
            (name.to_owned(), value)
        })
        .collect()
}

#[test]
fn the_copies_are_the_fakes_own_constants_in_its_order() {
    let ours: Vec<(String, String)> = FAKE_CONSTANTS
        .iter()
        .map(|(n, v)| ((*n).to_owned(), (*v).to_owned()))
        .collect();
    assert_eq!(
        ours,
        fakes_constants(),
        "a constant of crates/kourier-fake/src/threema.rs was added, renamed or changed: copy it"
    );
}

#[test]
fn the_ids_have_the_shape_of_one_and_no_value_is_inside_another() {
    assert!(valid_id(constant("OWN_ID").as_bytes()));
    assert!(valid_id(constant("PEER_ID").as_bytes()));
    assert_ne!(constant("OWN_ID"), constant("PEER_ID"));
    assert_eq!(constant("GROUP_ID_HEX").len(), 16);
    let values: Vec<String> = needles()
        .expect("needles")
        .iter()
        .map(|n| format!("{:?}", n))
        .collect();
    assert!(!values.is_empty());
    let all: Vec<String> = FAKE_CONSTANTS
        .iter()
        .map(|(_, v)| (*v).to_owned())
        .chain(
            ["OWN_ID", "PEER_ID"]
                .iter()
                .map(|n| digest_of_id(constant(n).as_bytes()).expect("a digest")),
        )
        .collect();
    for (i, a) in all.iter().enumerate() {
        for (j, b) in all.iter().enumerate() {
            assert!(i == j || !a.contains(b.as_str()), "{b} is inside {a}");
        }
    }
}

// ---- The search ----------------------------------------------------------------------------------------------

#[test]
fn a_hit_names_the_label_and_never_the_value() {
    let own = constant("OWN_ID");
    let found = hits(
        format!("a line with {own} in it").as_bytes(),
        &needles().expect("needles"),
    );
    assert_eq!(found, ["the fake's OWN_ID"]);
    let digest = digest_of_id(own.as_bytes()).expect("digest");
    let found = hits(
        format!("sha256:{digest}").as_bytes(),
        &needles().expect("needles"),
    );
    assert_eq!(found, ["the digest of the fake's OWN_ID"]);
    assert!(hits(b"nothing of the fake", &needles().expect("needles")).is_empty());
    for label in found {
        assert!(!label.contains(own) && !label.contains(&digest));
    }
}

#[test]
fn the_search_reads_every_file_but_the_stamped_program_and_flags_a_digest_shape_in_the_tools_files()
{
    let s = Scratch::new("r-leak");
    let write = |rel: &str, text: &str| {
        let path = s.0.join(rel);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
        std::fs::write(path, text).expect("file");
    };
    let own = constant("OWN_ID");
    let peer_name = constant("PEER_NAME");
    // The stamped program and its stamp: the program holds the names as code, the stamp the program's hash.
    write(
        "kourier-threema-run/p/threema-run/bin/kourier",
        &format!("{peer_name} {own}"),
    );
    write(
        "kourier-threema-run/p/threema-run/bin/stamp.json",
        &digest_shape(),
    );
    // A tool file with an ID, one with a digest-shaped run, a clean one.
    write(
        "kourier-threema-run/p/threema-run/app.log",
        &format!("x {own} y"),
    );
    write(
        "kourier-threema-run/p/threema-run/steps.jsonl",
        &digest_shape(),
    );
    write(
        "kourier-threema-run/p/threema-run/state.json",
        "{\"starts\": 2}",
    );
    // The app's own data: a name is a finding, a digest-shaped run is not looked for there.
    write("kourier-threema-run/p/kourier/data/log.txt", peer_name);
    write(
        "kourier-threema-run/p/kourier/data/core.db",
        &digest_shape(),
    );
    // The keeper root's own file.
    write("kourier-threema-run/note", &digest_shape());
    let mut found = leaks_in(&s.0, &needles().expect("needles"));
    found.sort();
    let names: Vec<&str> = found.iter().map(|(f, _)| f.as_str()).collect();
    assert_eq!(
        names,
        [
            "kourier-threema-run/note",
            "kourier-threema-run/p/kourier/data/log.txt",
            "kourier-threema-run/p/threema-run/app.log",
            "kourier-threema-run/p/threema-run/steps.jsonl",
        ],
        "{found:?}"
    );
    for (file, what) in &found {
        assert!(
            !what.contains(own) && !what.contains(peer_name) && !file.contains(own),
            "{what}"
        );
    }
}

#[test]
fn the_redaction_markers_are_counted() {
    assert_eq!(redaction_markers("a [id] b [digest] c [id]"), (2, 1));
    assert_eq!(redaction_markers("nothing"), (0, 0));
}

// ---- Reading what the commands printed ---------------------------------------------------------------------------

#[test]
fn a_steps_own_verdict_line_is_read_with_its_reason() {
    let printed = "STEP link on profile p (rehearsal): x\nNOTE: a\nPASS link (12.3 s)\n";
    let v = verdict_of(printed, "link").expect("a verdict");
    assert!(v.pass && v.reason.is_empty());
    let printed =
        "FAIL unlink (0.4 s): you said you do not want the link ended now: nothing was changed\n";
    let v = verdict_of(printed, "unlink").expect("a verdict");
    assert!(
        !v.pass && v.reason.starts_with("you said you do not want"),
        "{v:?}"
    );
    assert_eq!(verdict_of("PASS linkage (1.0 s)\n", "link"), None);
    assert_eq!(verdict_of("PASS link (1.0 s)\n", "sync"), None);
    assert_eq!(verdict_of("no verdict at all\n", "link"), None);
    assert_eq!(
        line_with_for_tests("  probe:      yes\n", "probe:"),
        Some("probe:      yes")
    );
}

#[test]
fn the_probes_three_lists_are_read_by_their_keys() {
    let out = "networks registered=Fake,Threema off= on=Threema\n";
    assert_eq!(
        kinds_of(out, "registered="),
        Some(vec!["Fake".to_owned(), "Threema".to_owned()])
    );
    assert_eq!(kinds_of(out, "off="), Some(Vec::new()));
    assert_eq!(kinds_of(out, "on="), Some(vec!["Threema".to_owned()]));
    assert_eq!(kinds_of("no networks line\n", "on="), None);
}

#[test]
fn a_command_run_in_a_namespace_keeps_its_arguments_and_its_environment_changes() {
    let mut cmd = Command::new("/bin/true");
    cmd.arg("a")
        .arg("b")
        .env("X_KEPT", "1")
        .env_remove("X_GONE");
    let wrapped = in_unshare(&cmd);
    assert_eq!(wrapped.get_program(), "unshare");
    let args: Vec<_> = wrapped
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert_eq!(args, ["-rn", "/bin/true", "a", "b"]);
    let envs: Vec<_> = wrapped.get_envs().collect();
    assert!(envs.iter().any(|(k, v)| *k == "X_KEPT" && v.is_some()));
    assert!(envs.iter().any(|(k, v)| *k == "X_GONE" && v.is_none()));
}

// ---- The places ----------------------------------------------------------------------------------------------------

#[test]
fn a_place_is_a_rehearsal_place_and_its_socket_directory_is_short_and_keyed_on_its_seconds() {
    let s = Scratch::new("r-place");
    let target = s.0.join("target");
    let place = place_under(&target, 1_730_000_123, "x", "p");
    let w = Where {
        profile: None,
        rehearsal_place: Some(place.clone()),
    };
    let dir = w
        .resolve(&|_| None, &target)
        .expect("a rehearsal place resolves");
    assert!(dir.rehearsal && dir.dir == place);
    let base = s.0.join("shm");
    std::fs::create_dir_all(&base).expect("base");
    let uid = crate::threema_run::tests::preflight::current_uid();
    let socket_dir = rehearsal_socket_dir(&dir, &target, &base, uid).expect("a socket directory");
    assert_eq!(
        socket_dir.file_name().and_then(|n| n.to_str()),
        Some(format!("kthr-{uid}-1730000123").as_str())
    );
    // The wipe place and the scratch keeper root are other data homes of the same seconds.
    assert_ne!(place_under(&target, 1_730_000_123, "w", "w"), place);
}

// ---- The terminal ------------------------------------------------------------------------------------------------

#[test]
fn what_is_typed_on_a_terminal_reaches_the_program_and_is_not_shown() {
    let mut cmd = Command::new("sh");
    cmd.args([
        "-c",
        "stty -echo </dev/tty; printf 'Secret: ' >/dev/tty; read x </dev/tty; echo \"got $x\"",
    ]);
    let secret = id_shape();
    let typed = format!("{secret}\n");
    let done = run_on_a_terminal(
        cmd,
        &[("Secret: ", typed.as_str())],
        Duration::from_secs(30),
    )
    .expect("the command ends");
    assert_eq!(done.code, Some(0));
    assert!(
        done.stdout.contains(&format!("got {secret}")),
        "{}",
        done.stdout
    );
    assert!(
        !done.screen.contains(&secret),
        "the terminal showed what was typed"
    );
    assert!(done.screen.contains("Secret: "));
}

#[test]
fn a_command_on_a_terminal_is_in_the_foreground_of_it_as_the_supervisors_rule_asks() {
    let mut cmd = Command::new("sh");
    cmd.args(["-c", "cat /proc/$$/stat"]);
    let done = run_on_a_terminal(cmd, &[], Duration::from_secs(30)).expect("ends");
    assert!(foreground_of_a_terminal(&done.stdout), "{}", done.stdout);
    // A command with no terminal of its own is not (the walk's own `cat`, stdin and all).
    let plain = Command::new("sh")
        .args(["-c", "cat /proc/$$/stat"])
        .stdin(std::process::Stdio::null())
        .output()
        .expect("runs");
    // (the test binary may itself run on a terminal of the user's: only the walk's case is asserted)
    let _ = plain;
}

#[test]
fn a_prompt_that_never_comes_ends_the_wait_and_kills_the_command() {
    let mut cmd = Command::new("sh");
    cmd.args(["-c", "sleep 60"]);
    let e = run_on_a_terminal(cmd, &[("Never: ", "x")], Duration::from_millis(600))
        .expect_err("no prompt");
    assert!(
        format!("{e:#}").contains("never showed the prompt"),
        "{e:#}"
    );
}

#[test]
fn a_command_that_refuses_ends_with_its_status_and_its_words() {
    let mut cmd = Command::new("sh");
    cmd.args(["-c", "echo said >&2; exit 3"]);
    let done = run_on_a_terminal(cmd, &[], Duration::from_secs(30)).expect("ends");
    assert_eq!(done.code, Some(3));
    assert!(done.stderr.contains("said"));
}

#[test]
fn the_first_prompt_of_id_ends_in_the_words_the_walk_waits_for() {
    assert!(PROMPT_FIRST.ends_with("then Enter: "));
}

// ---- The user the walk plays ------------------------------------------------------------------------------------------

#[test]
fn every_cue_is_one_the_supervisor_forwards_in_a_rehearsal_and_no_other_is() {
    for cue in [
        Cue::Click,
        Cue::Arrive("direct"),
        Cue::Arrive("group"),
        Cue::Send("direct"),
        Cue::Send("group"),
    ] {
        let r = cue_request(cue);
        assert_eq!(r["op"], "ctl");
        assert!(
            rehearsal_cue_ok(r["cmd"].as_str().expect("cmd"), &r["args"]),
            "{cue:?}"
        );
    }
    let no = [
        ("add_account", json!({"network": "Signal"})),
        ("add_account", json!({"network": "Threema", "x": 1})),
        ("add_account", json!({})),
        ("rehearsal_arrive", json!({"kind": "other"})),
        ("rehearsal_arrive", json!({"kind": "direct", "x": 1})),
        ("rehearsal_act", json!({"kind": "direct", "act": "pin"})),
        ("send", json!({"text": "x"})),
        ("send", json!({"text": "x", "conversation": 7})),
        ("send", json!({"text": REHEARSAL_SEND_TEXT})),
        (
            "send",
            json!({"text": REHEARSAL_SEND_TEXT, "conversation": 0}),
        ),
        (
            "send",
            json!({"text": REHEARSAL_SEND_TEXT, "conversation": "7"}),
        ),
        (
            "send",
            json!({"text": REHEARSAL_SEND_TEXT, "conversation": 7, "attachments": []}),
        ),
        ("page", json!({"name": "list"})),
        ("quit", json!({})),
    ];
    for (cmd, args) in no {
        assert!(!rehearsal_cue_ok(cmd, &args), "{cmd} {args}");
    }
}

fn cue_request(cue: Cue) -> serde_json::Value {
    crate::threema_run::rehearse::cue_request_for_tests(cue)
}

#[test]
fn a_plan_answers_the_questions_it_knows_and_none_it_does_not() {
    let plan = Plan {
        answers: vec![("end-go", false), ("emoji-equal", true)],
        ..Plan::default()
    };
    assert_eq!(plan.answer_for("end-go"), Some(false));
    assert_eq!(plan.answer_for("emoji-equal"), Some(true));
    assert_eq!(plan.answer_for("phone-warning"), None);
}

#[test]
fn the_rehearsals_step_timing_is_shorter_than_the_real_one_everywhere_a_wait_is_for_a_person() {
    let (r, real) = (StepTiming::REHEARSAL, StepTiming::REAL);
    assert!(r.poll <= real.poll);
    for (short, long) in [
        (r.call_max, real.call_max),
        (r.ready_wait, real.ready_wait),
        (r.qr_wait, real.qr_wait),
        (r.online_wait, real.online_wait),
        (r.answer_wait, real.answer_wait),
        (r.settle, real.settle),
        (r.baseline_wait, real.baseline_wait),
        (r.lost_after, real.lost_after),
        (r.read_wait, real.read_wait),
        (r.count_wait, real.count_wait),
        (r.d1_wait, real.d1_wait),
        (r.sleep_total, real.sleep_total),
        (r.removal_wait, real.removal_wait),
    ] {
        assert!(short < long, "{short:?} {long:?}");
    }
    // The answers' wait is longer than the walk's own pause between a question and its answer.
    assert!(r.answer_wait >= Duration::from_secs(30));
}

// ---- The list's life ----------------------------------------------------------------------------------------------------

#[test]
fn the_list_that_stood_is_judged_by_its_mode_its_root_and_its_one_line() {
    let digest = digest_of_id(constant("OWN_ID").as_bytes()).expect("digest");
    let good = Sighting {
        file_mode: 0o600,
        root_mode: 0o700,
        text: list_text(&digest),
    };
    judge_sighting(Some(&good), &digest).expect("a good list");
    let wrong = |s: Sighting| {
        judge_sighting(Some(&s), &digest)
            .expect_err("a refusal")
            .to_string()
    };
    assert!(
        wrong(Sighting {
            file_mode: 0o644,
            ..good.clone()
        })
        .contains("600")
    );
    assert!(
        wrong(Sighting {
            root_mode: 0o755,
            ..good.clone()
        })
        .contains("700")
    );
    assert!(
        wrong(Sighting {
            text: format!("{}x", good.text),
            ..good.clone()
        })
        .contains("one line")
    );
    assert!(
        wrong(Sighting {
            text: list_text(&digest_shape()),
            ..good.clone()
        })
        .contains("one line")
    );
    let none = judge_sighting(None, &digest)
        .expect_err("never seen")
        .to_string();
    assert!(none.contains("never seen"));
    // No refusal holds the digest it judged.
    assert!(
        !wrong(Sighting {
            file_mode: 0o644,
            ..good
        })
        .contains(&digest)
    );
}

// ---- What the end says ----------------------------------------------------------------------------------------------------

#[test]
fn the_end_line_says_what_the_walk_did_not_show() {
    for word in [
        "flight-mode",
        "phone-sleep",
        "kinds",
        "no leader line",
        "mirror",
        "wrong-ID refusal",
        "B1's gate",
        "raw stderr",
    ] {
        assert!(NOT_SHOWN.contains(word), "{word}");
    }
    // `backup` is walked since T10 (the stopped profile, a list beside it).
    assert!(!NOT_SHOWN.contains("backup"));
}
