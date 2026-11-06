//! The automated path of the steps (a run started with a test phone): the TEST phone's questions are
//! answered by checks the step makes on a scripted phone, the origins are written down, the consent
//! is a flag, the relayed answer is accepted only in such a run. The supervisor is the real one; the
//! phone is `tphone_fake`'s; nothing opens adb.

use std::cell::RefCell;
use std::rc::Rc;

use super::*;
use crate::test_phone::FakePhone;
use crate::threema_run::prompt::digest_of_id;
use crate::threema_run::tphone::{self, host_name};
use crate::threema_run::tphone_fake::{ThreemaFake, shell_handler};

fn own() -> String {
    id_shape()
}

fn phone_with(devices: Vec<(String, String)>) -> ThreemaFake {
    ThreemaFake {
        own_id: own(),
        devices,
        ..Default::default()
    }
}

fn kourier() -> Vec<(String, String)> {
    vec![(
        format!("Kourier ({})", host_name().unwrap()),
        "Linux".to_owned(),
    )]
}

/// The profile keeps a test phone (the value is a name; the phone given to the step is the fake).
fn with_test_phone(stage: &Stage) {
    let mut state = State::load(&stage.rig.dir)
        .expect("the state")
        .expect("a state file");
    state.set("test_phone", json!("fake:1"));
    state.save(&stage.rig.dir).expect("saved");
}

fn tell_digest(run: &Running, id: &str) {
    let digest = digest_of_id(id.as_bytes()).expect("a digest");
    run.call(&json!({"op": "test_digest", "digest": digest}))
        .expect("the digest is kept");
}

fn link_scenario(cues: &Cues, hear: &Hear) {
    hear.until("WINDOW:");
    cues.event(&step_event("qr", "qr"));
    cues.event(&qr_event());
    hear.until("scan now");
    cues.event(&step_event("emoji", "emoji"));
    hear.until("compare the three emoji");
    cues.event(&step_event("linking", "progress"));
    cues.event(&step_event("done", "done"));
    cues.event(&account_event("active", "syncing"));
}

fn origins_of(stage: &Stage) -> Vec<(String, String)> {
    stage
        .records()
        .iter()
        .flat_map(|r| {
            r["facts"]["origins"]
                .as_array()
                .cloned()
                .unwrap_or_default()
        })
        .map(|o| {
            (
                o["question"].as_str().unwrap_or_default().to_owned(),
                o["origin"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect()
}

fn origin(stage: &Stage, question: &str) -> String {
    origins_of(stage)
        .into_iter()
        .find(|(q, _)| q == question)
        .map(|(_, o)| o)
        .unwrap_or_default()
}

#[test]
fn link_with_a_test_phone_checks_the_phone_and_asks_only_what_it_cannot_look_at() {
    let stage = Stage::new("tp-link");
    with_test_phone(&stage);
    let run = stage.start(true);
    tell_digest(&run, &own());
    give_phone(phone_with(kourier()).build());
    // The window's emoji are not in the dev control's state: that one stays a question for a person.
    let end = drive(
        &stage,
        "link",
        &[],
        short(),
        &[("emoji-equal", true), ("phone-warning", false)],
        link_scenario,
    );
    assert_eq!(end.status, 0, "{}", end.reason);
    // No question for the TEST phone's facts. The emoji stays HIS, typed in his terminal (a
    // TERMINAL line, never ASK-USER: it is not relayable); the PERSONAL phone's warning half is an
    // ASK-USER.
    assert!(!said(&end, "Is the phone in your hand"), "{:?}", end.said);
    assert!(
        end.said
            .iter()
            .any(|l| l.starts_with("TERMINAL:") && l.contains("emoji")),
        "{:?}",
        end.said
    );
    assert!(
        !end.said
            .iter()
            .any(|l| l.starts_with("ASK-USER [") && l.contains("emoji"))
    );
    assert!(
        end.said
            .iter()
            .any(|l| l.starts_with("ASK-USER [") && l.contains("PERSONAL phone"))
    );
    assert!(said(&end, "DO-USER: scan now"), "{:?}", end.said);
    assert_eq!(
        origin(&stage, "test-phone-in-hand"),
        "checked on the TEST phone"
    );
    assert_eq!(
        origin(&stage, "list-shows-kourier"),
        "checked on the TEST phone"
    );
    assert_eq!(
        origin(&stage, "menu-path-as-runbook"),
        "checked on the TEST phone"
    );
    // H4: each half of the warning question has its own origin.
    assert_eq!(
        origin(&stage, "phone-warning (TEST phone half)"),
        "checked on the TEST phone"
    );
    assert_eq!(origin(&stage, "phone-warning"), "typed by the user");
    assert_eq!(origin(&stage, "emoji-equal"), "typed by the user");
    nothing_of_a_value(&end, &stage);
    assert!(run.info().expect("info")["halted"].is_null());
    finish(&stage, run);
}

#[test]
fn link_a_phone_that_shows_another_id_is_not_the_test_phone_and_nothing_is_scanned() {
    let stage = Stage::new("tp-link-other");
    with_test_phone(&stage);
    let run = stage.start(true);
    tell_digest(&run, &own());
    // B1: the SUPERVISOR reads the phone: it shows another ID than the one the run holds
    stage.rig.phone.lock().unwrap().own_id = "SYNTHOTH".into();
    give_phone(phone_with(kourier()).build());
    let end = drive(&stage, "link", &[], short(), &[], |_, hear| {
        hear.until("WINDOW:");
    });
    assert_eq!(end.status, 1);
    assert!(end.reason.contains("NOT the TEST phone"), "{}", end.reason);
    assert!(!said(&end, "scan now"), "{:?}", end.said);
    assert!(run.info().expect("info")["halted"].is_null());
    finish(&stage, run);
}

#[test]
fn link_an_unreadable_my_id_decides_nothing_and_scans_nothing() {
    let stage = Stage::new("tp-link-unreadable");
    with_test_phone(&stage);
    let run = stage.start(true);
    tell_digest(&run, &own());
    stage.rig.phone.lock().unwrap().own_id = String::new();
    give_phone(phone_with(kourier()).build());
    let end = drive(&stage, "link", &[], short(), &[], |_, hear| {
        hear.until("WINDOW:");
    });
    assert_eq!(end.status, 1);
    assert!(
        end.reason.contains("run `step link` again"),
        "{}",
        end.reason
    );
    assert!(!said(&end, "scan now"));
    assert!(run.info().expect("info")["halted"].is_null());
    finish(&stage, run);
}

#[test]
fn link_an_unlabelled_row_is_a_finding_in_fixed_words_not_a_halt() {
    let stage = Stage::new("tp-link-unlabelled");
    with_test_phone(&stage);
    let run = stage.start(true);
    tell_digest(&run, &own());
    give_phone(
        phone_with(vec![(
            "Device".into(),
            "Information could not be obtained".into(),
        )])
        .build(),
    );
    let end = drive(
        &stage,
        "link",
        &[],
        short(),
        &[("emoji-equal", true), ("phone-warning", false)],
        link_scenario,
    );
    assert_eq!(end.status, 1);
    assert!(
        end.reason.contains("does NOT show a Kourier entry"),
        "{}",
        end.reason
    );
    assert!(end.notes.iter().any(|n| n.contains("a row is unlabelled")));
    assert!(run.info().expect("info")["halted"].is_null());
    finish(&stage, run);
}

#[test]
fn link_a_warning_in_the_phones_shade_halts_the_run() {
    let stage = Stage::new("tp-link-warn");
    with_test_phone(&stage);
    let run = stage.start(true);
    tell_digest(&run, &own());
    give_phone(
        ThreemaFake {
            warning_in_shade: true,
            ..phone_with(kourier())
        }
        .build(),
    );
    let end = drive(
        &stage,
        "link",
        &[],
        short(),
        &[("emoji-equal", true), ("phone-warning", false)],
        link_scenario,
    );
    assert_eq!(end.status, 1);
    assert!(end.reason.contains("phone-warning"), "{}", end.reason);
    finish_halted(run, "phone-warning", &stage);
}

#[test]
fn an_end_step_with_a_test_phone_needs_the_flag_and_changes_nothing_without_it() {
    let stage = Stage::new("tp-end");
    stage.linked();
    with_test_phone(&stage);
    let mut run = stage.start(false);
    online(&stage, &mut run);
    give_phone(phone_with(kourier()).build());
    let end = call_step(&stage, "unlink", &[], short(), &|_| {});
    assert_eq!(end.status, 1);
    assert!(end.reason.contains("--user-said-yes"), "{}", end.reason);
    let info = run.info().expect("info");
    assert_eq!(info["removal"], json!(false), "{info}");
    assert!(info["question"].is_null());
    // With the flag the lead passes after his word, the origin is "relayed by the lead".
    give_phone(phone_with(kourier()).build());
    let timing = StepTiming {
        removal_wait: Duration::from_millis(300),
        ..short()
    };
    let end = call_step(&stage, "unlink", &["--user-said-yes"], timing, &|_| {});
    assert!(
        end.notes.iter().any(|n| n.contains("relayed by the lead")),
        "{:?}",
        end.notes
    );
    assert!(end.said.iter().any(|l| l.starts_with("WINDOW:")));
    finish(&stage, run);
}

#[test]
fn a_relayed_answer_is_taken_only_in_a_run_with_a_test_phone_and_names_its_origin() {
    // without a test phone: refused, and the question stays open
    let stage = Stage::new("tp-relay-no");
    let run = stage.start(false);
    run.call(&json!({"op": "ask", "name": "pp-read-it"}))
        .expect("ask");
    let e = run
        .call(&json!({"op": "relay_answer", "name": "pp-read-it", "yes": true}))
        .expect_err("refused");
    assert!(format!("{e:#}").contains("no test phone"), "{e:#}");
    assert_eq!(run.info().expect("info")["question"], json!("pp-read-it"));
    let e = run
        .call(&json!({"op": "allow_phone", "digest": digest_shape()}))
        .expect_err("refused");
    assert!(format!("{e:#}").contains("no test phone"), "{e:#}");
    let e = run.call(&json!({"op": "hand"})).expect_err("refused");
    assert!(format!("{e:#}").contains("no test phone"), "{e:#}");
    finish(&stage, run);
    // with one: taken, origin relayed; a typed one says typed
    let stage = Stage::new("tp-relay-yes");
    with_test_phone(&stage);
    let run = stage.start(false);
    run.call(&json!({"op": "ask", "name": "pp-read-it"}))
        .expect("ask");
    run.call(&json!({"op": "relay_answer", "name": "pp-read-it", "yes": true}))
        .expect("taken");
    let got = run
        .call(&json!({"op": "answer_of", "name": "pp-read-it"}))
        .expect("answer_of");
    assert_eq!(
        (got["answer"].clone(), got["origin"].clone()),
        (json!(true), json!("relayed"))
    );
    run.call(&json!({"op": "ask", "name": "push-own"}))
        .expect("ask");
    run.call(&json!({"op": "test_answer", "name": "push-own", "yes": false}))
        .expect("typed");
    let got = run
        .call(&json!({"op": "answer_of", "name": "push-own"}))
        .expect("answer_of");
    assert_eq!(got["origin"], json!("typed"));
    finish(&stage, run);
}

fn nonces(words: &[&str]) {
    tphone::TEST_NONCES.with(|q| *q.borrow_mut() = words.iter().map(|w| (*w).to_owned()).collect());
}

#[test]
fn mirror_with_a_test_phone_sends_a_unique_text_itself_and_reads_the_read_state_from_kourier() {
    let stage = Stage::new("tp-mirror");
    stage.linked();
    with_test_phone(&stage);
    stage.cues.chats(&[
        chat(7, "direct", 1, 1000, "none"),
        chat(8, "group", 0, 500, "none"),
    ]);
    let mut run = stage.start(false);
    online(&stage, &mut run);
    nonces(&["abcdef"]);
    give_phone(
        ThreemaFake {
            sent_text: tphone::marker_for("mirror", "abcdef"),
            ..phone_with(kourier())
        }
        .build(),
    );
    let end = drive(
        &stage,
        "mirror",
        &[],
        short(),
        &[("phone-warning", false)],
        |cues, hear| {
            hear.until("ACT:");
            cues.chats(&[
                chat(7, "direct", 1, 2000, "sent"),
                chat(8, "group", 0, 500, "none"),
            ]);
            hear.until("DO-USER:");
            cues.chats(&[
                chat(7, "direct", 1, 2000, "read"),
                chat(8, "group", 0, 500, "none"),
            ]);
        },
    );
    assert_eq!(end.status, 0, "{} {:?}", end.reason, end.said);
    assert!(!end.said.iter().any(|l| l.starts_with("PHONE:")));
    assert_eq!(
        origin(&stage, "pp-read-it"),
        "checked in Kourier's own state"
    );
    nothing_of_a_value(&end, &stage);
    finish(&stage, run);
}

#[test]
fn mirror_after_the_send_tap_a_failure_keeps_the_carry_and_the_next_call_only_counts_m3() {
    let stage = Stage::new("tp-mirror-carry");
    stage.linked();
    with_test_phone(&stage);
    stage.cues.chats(&[chat(7, "direct", 1, 1000, "none")]);
    let mut run = stage.start(false);
    online(&stage, &mut run);
    nonces(&["abcdef"]);
    // the text does not show after the send: a finding, but the send WAS tapped
    give_phone(
        ThreemaFake {
            send_shows_text: false,
            sent_text: "kourier-rt1-mirror-abcdef".into(),
            ..phone_with(kourier())
        }
        .build(),
    );
    let first = call_step(&stage, "mirror", &[], short(), &|_| {});
    assert_eq!(first.status, 1, "{}", first.reason);
    let record = stage.records().pop().expect("a record");
    assert_eq!(record["resume"]["send_tapped"], json!(true), "{record}");
    assert_eq!(
        record["resume"]["marker_mirror"],
        json!("kourier-rt1-mirror-abcdef")
    );
    // The next call: the text shows (the phone caught up). It must NOT type or send again: with the
    // text standing in the chat a second prepare would fail, so a pass proves it only counted.
    give_phone(
        ThreemaFake {
            direct_texts: vec!["kourier-rt1-mirror-abcdef".into()],
            ..phone_with(kourier())
        }
        .build(),
    );
    stage.cues.chats(&[chat(7, "direct", 1, 2000, "read")]);
    let second = drive(
        &stage,
        "mirror",
        &[],
        short(),
        &[("phone-warning", false)],
        |_, _| {},
    );
    assert_eq!(second.status, 0, "{} {:?}", second.reason, second.said);
    assert!(
        !second
            .said
            .iter()
            .any(|l| l.starts_with("ACT:") && l.contains("typed")),
        "{:?}",
        second.said
    );
    finish(&stage, run);
}

/// A phone with a shell: wakefulness, Threema's uid and the two tables (shared with the test).
fn shell_phone(fake: ThreemaFake, tables: Rc<RefCell<(Vec<String>, Vec<String>)>>) -> FakePhone {
    fake.build().with_shell(shell_handler(
        Rc::new(RefCell::new("Asleep".to_owned())),
        10194,
        tables,
    ))
}

type Tables = Rc<RefCell<(Vec<String>, Vec<String>)>>;

fn tables() -> Tables {
    Rc::new(RefCell::new((Vec::new(), Vec::new())))
}

const D1: &str = "2026-11-04T10:00:01.000000Z  INFO kourier_threema::conn: incoming messages were unblocked as the leader";

fn flight_stage(name: &str) -> (Stage, Running) {
    let stage = Stage::new(name);
    stage.linked();
    with_test_phone(&stage);
    stage.cues.chats(&[
        chat(1, "direct", 0, 1000, "none"),
        chat(2, "group", 0, 900, "none"),
    ]);
    let mut run = stage.start(false);
    online(&stage, &mut run);
    (stage, run)
}

const FLIGHT_TYPED: [(&str, bool); 3] = [
    ("pp-delivered", true),
    ("fs-line", false),
    ("phone-warning", false),
];

fn two(cues: &Cues, direct: (i64, i64), group: (i64, i64)) {
    cues.chats(&[
        chat(1, "direct", direct.0, direct.1, "none"),
        chat(2, "group", group.0, group.1, "none"),
    ]);
}

#[test]
fn flight_mode_cuts_the_phone_even_when_kourier_already_leads_h1_and_counts_the_unique_texts() {
    let (stage, mut run) = flight_stage("tp-flight-lead");
    stage.cues.line(D1);
    run.wait_for("the leader", |i| i["leader"] == json!(true));
    nonces(&["cutaaa", "dirbbb", "grpccc", "latddd"]);
    let t = tables();
    let cut_seen = Rc::new(RefCell::new(0usize));
    let seen = Rc::clone(&cut_seen);
    let t2 = Rc::clone(&t);
    let mut inner = shell_handler(Rc::new(RefCell::new("Asleep".into())), 10194, Rc::clone(&t));
    let phone = ThreemaFake {
        direct_texts: vec![
            tphone::marker_for("flight-direct", "dirbbb"),
            tphone::marker_for("later", "latddd"),
        ],
        group_texts: vec![tphone::marker_for("flight-group", "grpccc")],
        ..phone_with(kourier())
    }
    .build()
    .with_shell(move |root, l| {
        if root && l.contains("nohup") {
            *seen.borrow_mut() += 1;
            // the cut is in both tables right after the insert
            let out = inner(root, l)?;
            assert_eq!((t2.borrow().0.len(), t2.borrow().1.len()), (1, 1));
            return Ok(out);
        }
        inner(root, l)
    });
    give_phone(phone);
    let no_count = StepTiming {
        count_wait: Duration::from_millis(300),
        later_text_after: Duration::from_millis(100),
        ..short()
    };
    let end = drive(
        &stage,
        "flight-mode",
        &[],
        no_count,
        &FLIGHT_TYPED,
        |cues, hear| {
            hear.until("kourier-rt1-flight-direct-dirbbb");
            two(cues, (1, 2000), (0, 900));
            hear.until("kourier-rt1-flight-group-grpccc");
            two(cues, (1, 2000), (1, 1900));
            hear.until("kourier-rt1-later-latddd");
            two(cues, (2, 3000), (1, 1900));
        },
    );
    assert_eq!(end.status, 0, "{} {:?}", end.reason, end.said);
    assert_eq!(*cut_seen.borrow(), 1, "the cut was made once");
    assert_eq!(
        (t.borrow().0.len(), t.borrow().1.len()),
        (0, 0),
        "and removed again"
    );
    assert!(
        end.said
            .iter()
            .any(|l| l.starts_with("ACT:") && l.contains("cut off the network"))
    );
    let notes = end.notes.join("\n");
    assert!(
        notes.contains("the cut was in place until the step removed it"),
        "{notes}"
    );
    // B3: the delivery of THAT text is a question for a person, never a counter
    assert!(
        end.said
            .iter()
            .any(|l| l.starts_with("ASK-USER [pp-delivered]: ") && l.contains("delivered"))
    );
    assert!(
        end.said.iter().any(|l| l.starts_with("ANSWER: ")
            && l.contains("answer --relayed y|n --question pp-delivered --profile")),
        "{:?}",
        end.said
    );
    assert_eq!(origin(&stage, "pp-delivered"), "typed by the user");
    assert_eq!(
        origin(&stage, "flight-off-both"),
        "checked on the TEST phone"
    );
    nothing_of_a_value(&end, &stage);
    finish(&stage, run);
}

#[test]
fn flight_mode_a_cut_that_ended_early_fails_the_step_h2() {
    let (stage, mut run) = flight_stage("tp-flight-early");
    stage.cues.line(D1);
    run.wait_for("the leader", |i| i["leader"] == json!(true));
    nonces(&["cutaaa", "dirbbb", "grpccc", "latddd"]);
    let t = tables();
    let fired = Arc::new(AtomicBool::new(false));
    let f2 = Arc::clone(&fired);
    let t2 = Rc::clone(&t);
    let mut inner = shell_handler(Rc::new(RefCell::new("Asleep".into())), 10194, Rc::clone(&t));
    give_phone(
        ThreemaFake {
            direct_texts: vec![tphone::marker_for("flight-direct", "dirbbb")],
            group_texts: vec![tphone::marker_for("flight-group", "grpccc")],
            ..phone_with(kourier())
        }
        .build()
        .with_shell(move |root, l| {
            // the phone's timer fires: both tables lose the rules
            if f2.load(Ordering::SeqCst) {
                let mut t = t2.borrow_mut();
                t.0.clear();
                t.1.clear();
            }
            inner(root, l)
        }),
    );
    let end = drive(
        &stage,
        "flight-mode",
        &[],
        short(),
        &FLIGHT_TYPED,
        move |cues, hear| {
            hear.until("kourier-rt1-flight-direct-dirbbb");
            fired.store(true, Ordering::SeqCst);
            two(cues, (1, 2000), (0, 900));
        },
    );
    assert_eq!(end.status, 1);
    assert!(end.reason.contains("cut ENDED"), "{}", end.reason);
    // no PASS and no halt
    assert!(run.info().expect("info")["halted"].is_null());
    finish(&stage, run);
}

#[test]
fn flight_mode_an_unreachable_phone_at_a_calls_start_is_a_pause_that_keeps_the_carry_m1() {
    let (stage, mut run) = flight_stage("tp-flight-unreach");
    stage.cues.line(D1);
    run.wait_for("the leader", |i| i["leader"] == json!(true));
    nonces(&["cutaaa", "dirbbb", "grpccc", "latddd"]);
    let t = tables();
    give_phone(shell_phone(
        ThreemaFake {
            ..phone_with(kourier())
        },
        Rc::clone(&t),
    ));
    // The first call cuts, then waits for the 1:1 text and runs out of time: a pause with the cut.
    let brief = StepTiming {
        call_max: Duration::from_millis(1500),
        lost_after: Duration::from_secs(30),
        ..short()
    };
    let first = call_step(&stage, "flight-mode", &[], brief, &|_| {});
    assert_eq!(first.status, 1, "{}", first.reason);
    let record = stage.records().pop().expect("a record");
    assert!(record["resume"]["cut_nonce"].is_string(), "{record}");
    assert_eq!(
        (t.borrow().0.len(), t.borrow().1.len()),
        (1, 1),
        "the cut stands"
    );
    // The next call cannot reach the phone: still a pause, the carry (and the cut on record) is kept.
    give_no_phone();
    let second = call_step(&stage, "flight-mode", &[], short(), &|_| {});
    assert_eq!(second.status, 1);
    assert!(
        second.reason.contains("could not be reached"),
        "{}",
        second.reason
    );
    let record = stage.records().pop().expect("a record");
    assert_eq!(record["resume"]["cut_nonce"], json!("cutaaa"), "{record}");
    finish(&stage, run);
}

#[test]
fn phone_sleep_counts_its_unique_text_and_an_unreadable_push_setting_is_not_looked_at_m2() {
    let (stage, run) = flight_stage("tp-sleep");
    nonces(&["slpaaa"]);
    let t = tables();
    give_phone(shell_phone(
        ThreemaFake {
            direct_texts: vec![tphone::marker_for("sleep", "slpaaa")],
            notifs_unreadable: true,
            ..phone_with(kourier())
        },
        t,
    ));
    let end = drive(
        &stage,
        "phone-sleep",
        &[],
        short(),
        &[("phone-warning", false)],
        |cues, hear| {
            hear.until("kourier-rt1-sleep-slpaaa");
            two(cues, (1, 2000), (0, 900));
        },
    );
    assert_eq!(end.status, 0, "{} {:?}", end.reason, end.said);
    assert_eq!(origin(&stage, "phone-locked"), "checked on the TEST phone");
    assert_eq!(origin(&stage, "tp-shows-once"), "checked on the TEST phone");
    assert_eq!(origin(&stage, "push-own"), "not looked at");
    assert!(
        end.notes
            .iter()
            .any(|n| n.contains("push mode: not looked at"))
    );
    finish(&stage, run);
}

/// RT1: the push setting is read ONCE per step. Call 1 cannot read it ("not looked at") and pauses at the
/// warning question; calls 2 and 3 resume and must keep "not looked at", never turn it into an answer he
/// gave.
#[test]
fn a_resumed_phone_sleep_keeps_the_push_mode_not_looked_at_and_never_calls_it_given_rt1() {
    let (stage, run) = flight_stage("tp-sleep-resume");
    nonces(&["slpaaa"]);
    let t = tables();
    let unreadable = || {
        shell_phone(
            ThreemaFake {
                direct_texts: vec![tphone::marker_for("sleep", "slpaaa")],
                notifs_unreadable: true,
                ..phone_with(kourier())
            },
            Rc::clone(&t),
        )
    };
    let waiting = StepTiming {
        answer_wait: Duration::from_millis(300),
        ..short()
    };
    give_phone(unreadable());
    let first = drive(&stage, "phone-sleep", &[], waiting, &[], |cues, hear| {
        hear.until("kourier-rt1-sleep-slpaaa");
        two(cues, (1, 2000), (0, 900));
    });
    assert_eq!(first.status, 1, "{}", first.reason);
    assert!(first.reason.contains("phone-warning"), "{}", first.reason);
    assert!(
        first
            .notes
            .iter()
            .any(|n| n.contains("push mode: not looked at")),
        "{:?}",
        first.notes
    );
    // call 2 pauses again at the warning, call 3 answers it
    give_phone(unreadable());
    let second = drive(&stage, "phone-sleep", &[], waiting, &[], |_, _| {});
    assert_eq!(second.status, 1, "{}", second.reason);
    give_phone(unreadable());
    let third = drive(
        &stage,
        "phone-sleep",
        &[],
        short(),
        &[("phone-warning", false)],
        |_, _| {},
    );
    assert_eq!(third.status, 0, "{}", third.reason);
    for (n, end) in [("second", &second), ("third", &third)] {
        let all = end.notes.join("\n");
        assert!(!all.contains("he gave"), "{n}: {all}");
        assert!(!all.contains("push mode ("), "{n}: {all}");
    }
    let last = stage.records().pop().expect("a record");
    assert_eq!(last["facts"]["obs"]["push_own"], Value::Null, "{last}");
    assert_eq!(
        last["facts"]["obs"]["push_origin"],
        json!("not looked at"),
        "{last}"
    );
    for r in stage.records() {
        if r["name"] == "phone-sleep" {
            assert_ne!(r["facts"]["obs"]["push_own"], json!(false), "{r}");
        }
    }
    finish(&stage, run);
}

/// RT1 (b) and (c): the own marker is missing and a marker of another step shows more often than that
/// step recorded: nothing is decided (no halt, "run `step <name>` again"), the note names that step. The
/// DO-USER line puts the marker on a line of its own.
#[test]
fn another_steps_marker_in_the_chat_is_a_mixup_not_lost_and_the_marker_has_its_own_line_rt1() {
    let (stage, run) = flight_stage("tp-sleep-mixup");
    nonces(&["slpaaa"]);
    let other = tphone::marker_for("mirror", "mirxxx");
    crate::threema_run::profile::append_line(
        &stage.rig.dir.steps_log(),
        &json!({
            "name": "mirror", "result": "PASS", "reason": "", "started_ms": 1, "ended_ms": 2,
            "notes": [], "resume": null,
            "facts": {"obs": {"markers": [{"step": "mirror", "marker": other, "seen": 0}]}},
        })
        .to_string(),
    )
    .expect("a record");
    give_phone(shell_phone(
        ThreemaFake {
            direct_texts: vec![other.clone()],
            ..phone_with(kourier())
        },
        tables(),
    ));
    let end = drive(
        &stage,
        "phone-sleep",
        &[],
        short(),
        &[("phone-warning", false)],
        |cues, hear| {
            hear.until("kourier-rt1-sleep-slpaaa");
            two(cues, (1, 2000), (0, 900));
        },
    );
    assert_eq!(end.status, 1, "{}", end.reason);
    assert!(end.reason.contains("a mix-up"), "{}", end.reason);
    assert!(
        end.reason.contains("run `step phone-sleep` again"),
        "{}",
        end.reason
    );
    assert!(!end.reason.contains("lost"), "{}", end.reason);
    assert!(
        end.notes
            .iter()
            .any(|n| n.contains("marker of step `mirror`")),
        "{:?}",
        end.notes
    );
    assert!(
        !end.notes.join("\n").contains("kourier-rt1-"),
        "{:?}",
        end.notes
    );
    // (c) a DO-USER line ending in the colon, then the marker alone on the next line
    let line = end
        .said
        .iter()
        .find(|l| l.starts_with("DO-USER: ") && l.contains("kourier-rt1-sleep-slpaaa"))
        .expect("the DO-USER line");
    let mut parts = line.lines();
    assert!(
        parts
            .next()
            .expect("first")
            .ends_with("The text must be exactly:"),
        "{line}"
    );
    assert_eq!(parts.next(), Some("kourier-rt1-sleep-slpaaa"), "{line}");
    assert_eq!(parts.next(), None, "{line}");
    // the run is not halted, and the extra text now counts as recorded
    assert!(stage.rig.dir.steps_log().exists());
    let markers = stage.records().pop().expect("a record")["facts"]["obs"]["markers"].clone();
    assert!(
        markers.as_array().is_some_and(|m| m
            .iter()
            .any(|e| e["marker"] == json!(other) && e["seen"] == json!(1))),
        "{markers}"
    );
    finish(&stage, run);
}
