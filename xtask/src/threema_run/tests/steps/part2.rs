//! The steps, part two (T7): `kinds`, `phone-sleep`, `flight-mode`, `removed-on-phone` and
//! `unlink`, one test per step and per stop-rule exit each can meet (S7 `lost` and `twice`, S9
//! `phone-warning`, a halt read by a step, the setter of `removal_asked`). The stand-in app, the
//! cues, the answerer and the step runner are part one's (`super`): the app is a shell script under
//! the REAL supervisor, the test is him (answers through the supervisor's test door) and the phones
//! (cue files). Nothing starts the real app, nothing contacts a host, no window, no sound. Every wait
//! is on an outcome (a line a step said, an answer, a thread's end); the bounds are failures only. No
//! mutant.

use super::*;

const D4_ACKED: &str = "2026-11-04T10:00:04.000000Z  INFO kourier_threema::conn: the mediator acked the own device's drop";
const D4_NOT_REACHED: &str = "2026-11-04T10:00:05.000000Z  INFO kourier_threema::conn::drop_own: the own device's drop on a socket of its own outcome=NotReached";
const E1_4113: &str = "2026-11-04T10:00:02.000000Z  INFO kourier_threema::conn: the mediator closed the connection code=4113 meaning=\"Removed\"";

/// The count line of the stand-in with some fields set (`when` included).
fn count_line(over: &[(&str, &str)]) -> String {
    COUNT_ZERO
        .split(' ')
        .map(|token| match token.split_once('=') {
            Some((key, _)) if over.iter().any(|(k, _)| *k == key) => {
                let value = over.iter().find(|(k, _)| *k == key).map_or("0", |(_, v)| v);
                format!("{key}={value}")
            }
            _ => token.to_owned(),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// A leader's count line with both texts of `flight-mode` in it (inside S5b's bounds).
fn leader_counts() -> String {
    count_line(&[
        ("in_csp_message", "2"),
        ("out_unblock", "1"),
        ("out_reflect_inbound", "2"),
        ("out_message_ack", "2"),
        ("out_box_job", "2"),
        ("out_reflect_job", "4"),
    ])
}

fn two_chats(cues: &Cues, direct: (i64, i64), group: (i64, i64)) {
    cues.chats(&[
        chat(1, "direct", direct.0, direct.1, "none"),
        chat(2, "group", group.0, group.1, "none"),
    ]);
}

/// A linked profile with a stand-in app that is online and lists one 1:1 chat and one group.
fn online_stage(name: &str) -> (Stage, Running) {
    let stage = Stage::new(name);
    stage.linked();
    two_chats(&stage.cues, (0, 1000), (0, 900));
    let mut run = stage.start(false);
    online(&stage, &mut run);
    (stage, run)
}

fn notes_of(end: &StepEnd) -> String {
    end.notes.join("\n")
}

fn obs(stage: &Stage, key: &str) -> Value {
    stage
        .records()
        .iter()
        .rev()
        .map(|r| r["facts"]["obs"][key].clone())
        .find(|v| !v.is_null())
        .unwrap_or(Value::Null)
}

/// The commands the app got are `chats` and `state` only: no chat opened, nothing sent.
fn read_only(stage: &Stage) {
    assert!(
        stage.seen().iter().all(|c| c == "chats" || c == "state"),
        "{:?}",
        stage.seen()
    );
}

// ---- kinds ---------------------------------------------------------------------------------------------------------

#[test]
fn kinds_counts_what_came_opens_no_chat_and_records_the_chats() {
    let (stage, run) = online_stage("t7-kinds");
    let answers = [("kinds-sent", true), ("phone-warning", false)];
    let end = drive_gated(
        &stage,
        "kinds",
        &[],
        short(),
        &answers,
        true,
        |cues, hear| {
            hear.until("in this order");
            two_chats(cues, (2, 2000), (1, 1900));
            hear.answers_now();
        },
    );
    assert_eq!(end.status, 0, "{}", end.reason);
    let notes = notes_of(&end);
    assert!(
        notes.contains("new messages: 2 in 1:1 chats, 1 in groups"),
        "{notes}"
    );
    assert!(
        notes.contains("chats whose activity moved: 1 1:1, 1 group"),
        "{notes}"
    );
    assert!(notes.contains("no chat was open"), "{notes}");
    assert!(notes.contains("no verdict per kind"), "{notes}");
    read_only(&stage);
    // The record has the chats for `reconnect` and what the report reads.
    let record = stage.records().pop().expect("a record");
    assert_eq!(record["result"], json!("PASS"));
    let facts = record["facts"]["chats"].as_array().expect("chats");
    assert_eq!(facts.len(), 2, "{record}");
    assert!(
        facts
            .iter()
            .any(|f| f["chat"] == json!(1) && f["unread"] == json!(2))
    );
    assert!(
        facts
            .iter()
            .any(|f| f["chat"] == json!(2) && f["unread"] == json!(1))
    );
    assert_eq!(obs(&stage, "new_direct"), json!(2));
    assert_eq!(obs(&stage, "new_group"), json!(1));
    assert_eq!(obs(&stage, "rows"), Value::Null);
    nothing_of_a_value(&end, &stage);
    assert!(run.info().expect("info")["halted"].is_null());
    finish(&stage, run);
}

#[test]
fn kinds_reads_the_rows_of_a_chat_he_opened_and_counts_edits_reactions_and_tombstones() {
    let stage = Stage::new("t7-kinds-open");
    stage.linked();
    stage.cues.chats(&[
        chat(1, "direct", 0, 1000, "none"),
        chat(2, "group", 0, 900, "none"),
    ]);
    stage.cues.put(
        "state.json",
        &json!({"open": 1, "rows": [
            {"message": 1, "own": false, "edited": false, "deleted": false, "quote": null, "reactions": []}
        ]}),
    );
    let mut run = stage.start(false);
    online(&stage, &mut run);
    let answers = [("kinds-sent", true), ("phone-warning", false)];
    let end = drive_gated(
        &stage,
        "kinds",
        &[],
        short(),
        &answers,
        true,
        |cues, hear| {
            hear.until("in this order");
            cues.put(
            "state.json",
            &json!({"open": 1, "rows": [
                {"message": 4, "own": false, "edited": false, "deleted": true, "quote": null, "reactions": []},
                {"message": 3, "own": false, "edited": true, "deleted": false, "quote": "found", "reactions": []},
                {"message": 1, "own": false, "edited": false, "deleted": false, "quote": null,
                 "reactions": [{"emoji": "x", "count": 2, "mine": false}]}
            ]}),
        );
            cues.chats(&[
                chat(1, "direct", 0, 2000, "none"),
                chat(2, "group", 0, 900, "none"),
            ]);
            hear.answers_now();
        },
    );
    assert_eq!(end.status, 0, "{}", end.reason);
    let notes = notes_of(&end);
    // Two new rows from others in the open chat (the unread count of an open chat stays 0).
    assert!(notes.contains("new messages: 2 in 1:1 chats"), "{notes}");
    assert!(
        notes
            .contains("quoted rows +1, edited rows +1, deleted rows (tombstones) +1, reactions +2"),
        "{notes}"
    );
    assert!(notes.contains("his act; the step opened none"), "{notes}");
    assert_eq!(obs(&stage, "rows")["reactions"], json!(2));
    finish(&stage, run);
}

#[test]
fn kinds_goes_on_in_a_second_call_and_nothing_arriving_is_its_one_finding() {
    let (stage, run) = online_stage("t7-kinds-resume");
    // He does not answer in this call: it ends FAIL with the baseline kept and the question open.
    let timing = StepTiming {
        call_max: Duration::from_millis(700),
        ..short()
    };
    let first = drive(&stage, "kinds", &[], timing, &[], |_, _| {});
    assert_eq!(first.status, 1);
    assert!(first.reason.contains("kinds-sent"), "{}", first.reason);
    assert!(
        first.reason.contains("run this step again"),
        "{}",
        first.reason
    );
    let record = stage.records().pop().expect("a record");
    assert!(record["resume"]["base"].is_object(), "{record}");
    assert_eq!(run.info().expect("info")["question"], json!("kinds-sent"));
    // The second call goes on from the baseline; nothing came: the one FAIL, no halt.
    let answers = [("kinds-sent", true), ("phone-warning", false)];
    let second = drive(&stage, "kinds", &[], short(), &answers, |_, _| {});
    assert_eq!(second.status, 1);
    assert!(
        second
            .notes
            .iter()
            .any(|n| n.contains("baseline of the earlier call")),
        "{:?}",
        second.notes
    );
    assert!(
        second.reason.contains("nothing new arrived"),
        "{}",
        second.reason
    );
    assert!(run.info().expect("info")["halted"].is_null());
    read_only(&stage);
    finish(&stage, run);
}

#[test]
fn kinds_s9_a_warning_halts_and_a_no_to_all_six_is_a_note() {
    let (stage, run) = online_stage("t7-kinds-s9");
    let answers = [("kinds-sent", false), ("phone-warning", true)];
    let end = drive_gated(
        &stage,
        "kinds",
        &[],
        short(),
        &answers,
        true,
        |cues, hear| {
            hear.until("in this order");
            two_chats(cues, (1, 2000), (0, 900));
            hear.answers_now();
        },
    );
    assert_eq!(end.status, 1);
    assert!(end.reason.contains("phone-warning"), "{}", end.reason);
    assert!(
        end.notes
            .iter()
            .any(|n| n.contains("not all six were sent")),
        "{:?}",
        end.notes
    );
    finish_halted(run, "phone-warning", &stage);
}

// ---- phone-sleep -----------------------------------------------------------------------------------------------

const SLEEP_ANSWERS: [(&str, bool); 4] = [
    ("phone-locked", true),
    ("tp-shows-once", true),
    ("push-own", true),
    ("phone-warning", false),
];

#[test]
fn phone_sleep_records_a_lead_taken_in_the_sleep_the_text_once_and_the_push_mode() {
    let (stage, mut run) = online_stage("t7-sleep-d1");
    let end = drive(
        &stage,
        "phone-sleep",
        &[],
        short(),
        &SLEEP_ANSWERS,
        |cues, hear| {
            hear.until("WAIT:");
            cues.line(D1_LINE);
            hear.until("PERSONAL phone: send ONE short text");
            two_chats(cues, (1, 2000), (0, 900));
        },
    );
    assert_eq!(end.status, 0, "{}", end.reason);
    let notes = notes_of(&end);
    assert!(
        notes.contains("leader observed in sleep: YES (D1 "),
        "{notes}"
    );
    assert!(notes.contains("the text came into Kourier once"), "{notes}");
    assert!(notes.contains("exactly once after the unlock"), "{notes}");
    assert!(
        notes.contains("push mode (typed by the user): Threema's own push service"),
        "{notes}"
    );
    assert_eq!(obs(&stage, "leader_in_sleep"), json!(true));
    assert_eq!(obs(&stage, "push_own"), json!(true));
    assert_eq!(obs(&stage, "tp_shows_once"), json!(true));
    let record = stage.records().pop().expect("a record");
    assert_eq!(record["facts"]["chats"][0]["unread"], json!(1));
    // The lead is the judge's business and was no halt: a D1 with no counts is within the bounds.
    run.wait_for("the leader", |i| i["leader"] == json!(true));
    assert!(run.info().expect("info")["halted"].is_null());
    nothing_of_a_value(&end, &stage);
    finish(&stage, run);
}

#[test]
fn phone_sleep_without_a_promotion_says_no_and_still_passes_and_a_lead_from_before_is_said() {
    let (stage, run) = online_stage("t7-sleep-no");
    let end = drive(
        &stage,
        "phone-sleep",
        &[],
        short(),
        &SLEEP_ANSWERS,
        |cues, hear| {
            hear.until("PERSONAL phone: send ONE short text");
            two_chats(cues, (1, 2000), (0, 900));
        },
    );
    assert_eq!(end.status, 0, "{}", end.reason);
    assert!(
        notes_of(&end).contains("leader observed in sleep: NO (no D1 in"),
        "{}",
        notes_of(&end)
    );
    assert_eq!(obs(&stage, "leader_in_sleep"), json!(false));
    finish(&stage, run);

    // Kourier led before the step: no second D1 can come, and the note says so.
    let (stage, mut run) = online_stage("t7-sleep-before");
    stage.cues.line(D1_LINE);
    run.wait_for("the leader", |i| i["leader"] == json!(true));
    let end = drive(
        &stage,
        "phone-sleep",
        &[],
        short(),
        &SLEEP_ANSWERS,
        |cues, hear| {
            hear.until("PERSONAL phone: send ONE short text");
            two_chats(cues, (1, 2000), (0, 900));
        },
    );
    assert_eq!(end.status, 0, "{}", end.reason);
    assert!(
        notes_of(&end).contains("Kourier led already before the step"),
        "{}",
        notes_of(&end)
    );
    assert_eq!(obs(&stage, "leader_at_start"), json!(true));
    finish(&stage, run);
}

#[test]
fn phone_sleep_starts_nothing_until_the_phone_is_locked_and_goes_on_from_the_clock_of_the_lock() {
    let (stage, run) = online_stage("t7-sleep-resume");
    // A no to "locked": nothing started, no resume.
    let end = drive(
        &stage,
        "phone-sleep",
        &[],
        short(),
        &[("phone-locked", false)],
        |_, _| {},
    );
    assert_eq!(end.status, 1);
    assert!(end.reason.contains("not locked"), "{}", end.reason);
    assert!(stage.records()[0]["resume"].is_null());
    // The sleep is longer than one call: the first call ends FAIL with the lock's time carried.
    let long = StepTiming {
        sleep_text_at: Duration::from_secs(3),
        call_max: Duration::from_millis(800),
        ..short()
    };
    let first = drive(&stage, "phone-sleep", &[], long, &SLEEP_ANSWERS, |_, _| {});
    assert_eq!(first.status, 1);
    assert!(
        first.reason.contains("run `step phone-sleep` again"),
        "{}",
        first.reason
    );
    let record = stage.records().pop().expect("a record");
    assert!(record["resume"]["locked_ms"].is_u64(), "{record}");
    assert_eq!(record["resume"]["answers"]["phone-locked"], json!(true));
    // The second call needs no new "locked" and no new answer to it.
    let second = drive(
        &stage,
        "phone-sleep",
        &[],
        short(),
        &SLEEP_ANSWERS,
        |cues, hear| {
            hear.until("PERSONAL phone: send ONE short text");
            two_chats(cues, (1, 2000), (0, 900));
        },
    );
    assert_eq!(second.status, 0, "{}", second.reason);
    assert!(
        !second.said.iter().any(|l| l.contains("LOCK it now")),
        "{:?}",
        second.said
    );
    finish(&stage, run);
}

#[test]
fn phone_sleep_s7_a_text_the_phone_misses_halts_and_s9_a_warning_halts() {
    let (stage, run) = online_stage("t7-sleep-s7");
    let mut answers = SLEEP_ANSWERS;
    answers[1] = ("tp-shows-once", false);
    let end = drive(
        &stage,
        "phone-sleep",
        &[],
        short(),
        &answers,
        |cues, hear| {
            hear.until("PERSONAL phone: send ONE short text");
            two_chats(cues, (1, 2000), (0, 900));
        },
    );
    assert_eq!(end.status, 1);
    assert!(end.reason.contains("halted (lost)"), "{}", end.reason);
    finish_halted(run, "lost", &stage);

    let (stage, run) = online_stage("t7-sleep-s9");
    let mut answers = SLEEP_ANSWERS;
    answers[3] = ("phone-warning", true);
    let end = drive(
        &stage,
        "phone-sleep",
        &[],
        short(),
        &answers,
        |cues, hear| {
            hear.until("PERSONAL phone: send ONE short text");
            two_chats(cues, (1, 2000), (0, 900));
        },
    );
    assert_eq!(end.status, 1);
    assert!(end.reason.contains("phone-warning"), "{}", end.reason);
    finish_halted(run, "phone-warning", &stage);
}

// ---- flight-mode -------------------------------------------------------------------------------------------------

const FLIGHT_ANSWERS: [(&str, bool); 6] = [
    ("flight-on", true),
    ("pp-delivered", true),
    ("flight-off-both", true),
    ("tp-shows-later", true),
    ("fs-line", true),
    ("phone-warning", false),
];

/// The phones and the server as far as `level` goes: 0 the promotion, 1 + the 1:1 text, 2 + the group
/// text and the leader's count line, 3 + the later text.
fn flight_play(level: u8) -> impl FnOnce(&Cues, &Hear) + Send + 'static {
    move |cues: &Cues, hear: &Hear| {
        hear.until("WAIT:");
        cues.line(D1_LINE);
        cues.event(&account_event("active", "syncing"));
        cues.event(&account_event("active", "online"));
        if level >= 1 {
            hear.until("(the TEST phone is offline)");
            two_chats(cues, (1, 2000), (0, 900));
        }
        if level >= 2 {
            hear.until("the TEST phone is still offline");
            two_chats(cues, (1, 2000), (1, 1900));
            cues.line(&leader_counts());
        }
        if level >= 3 {
            hear.until("ONE more short text");
            two_chats(cues, (2, 3000), (1, 1900));
        }
    }
}

#[test]
fn flight_mode_the_whole_leader_walk_counts_both_texts_and_the_later_one() {
    let (stage, run) = online_stage("t7-flight");
    let end = drive(
        &stage,
        "flight-mode",
        &[],
        short(),
        &FLIGHT_ANSWERS,
        flight_play(3),
    );
    assert_eq!(end.status, 0, "{}", end.reason);
    let notes = notes_of(&end);
    assert!(notes.contains("Kourier took the lead "), "{notes}");
    assert!(
        notes.contains("account states since the step began"),
        "{notes}"
    );
    assert!(
        notes.contains("the 1:1 text came into Kourier once"),
        "{notes}"
    );
    assert!(
        notes.contains("the group text came into Kourier once"),
        "{notes}"
    );
    assert!(
        notes.contains("the leader's count line: when=running"),
        "{notes}"
    );
    assert!(notes.contains("in_csp_message=2"), "{notes}");
    assert!(
        notes.contains("both texts once each after flight mode went off"),
        "{notes}"
    );
    assert!(
        notes.contains("the later text came into Kourier once"),
        "{notes}"
    );
    assert!(
        notes.contains("still leads at the end; the socket lived"),
        "{notes}"
    );
    assert!(
        notes.contains("shows a forward-security line (expected"),
        "{notes}"
    );
    assert_eq!(obs(&stage, "promoted"), json!(true));
    assert_eq!(obs(&stage, "leader_before"), json!(false));
    assert_eq!(obs(&stage, "fs_line"), json!(true));
    assert_eq!(obs(&stage, "pp_delivered"), json!(true));
    assert_eq!(obs(&stage, "leader_at_end"), json!(true));
    // The record carries both chats as the steps left them, for `reconnect`.
    let record = stage.records().pop().expect("a record");
    let chats = record["facts"]["chats"].as_array().expect("chats");
    assert!(
        chats
            .iter()
            .any(|c| c["chat"] == json!(1) && c["unread"] == json!(2))
    );
    assert!(
        chats
            .iter()
            .any(|c| c["chat"] == json!(2) && c["unread"] == json!(1))
    );
    // The step read and asked; it never sent a command of its own to the app.
    read_only(&stage);
    assert!(run.info().expect("info")["halted"].is_null());
    nothing_of_a_value(&end, &stage);
    finish(&stage, run);
}

#[test]
fn flight_mode_reads_the_role_first_a_lead_from_the_sleep_goes_straight_to_the_texts() {
    let (stage, mut run) = online_stage("t7-flight-lead");
    stage.cues.line(D1_LINE);
    run.wait_for("the leader", |i| i["leader"] == json!(true));
    // No "flight-on" answer is given: the step must not ask for one.
    let answers = [
        ("pp-delivered", true),
        ("flight-off-both", true),
        ("tp-shows-later", true),
        ("fs-line", false),
        ("phone-warning", false),
    ];
    let no_count = StepTiming {
        count_wait: Duration::from_millis(300),
        ..short()
    };
    let end = drive(
        &stage,
        "flight-mode",
        &[],
        no_count,
        &answers,
        |cues, hear| {
            hear.until("(the TEST phone is offline)");
            two_chats(cues, (1, 2000), (0, 900));
            hear.until("the TEST phone is still offline");
            two_chats(cues, (1, 2000), (1, 1900));
            hear.until("ONE more short text");
            two_chats(cues, (2, 3000), (1, 1900));
        },
    );
    assert_eq!(end.status, 0, "{}", end.reason);
    let notes = notes_of(&end);
    assert!(
        notes.contains("Kourier already leads before flight mode"),
        "{notes}"
    );
    assert!(
        !end.said.iter().any(|l| l.contains("FLIGHT MODE on")),
        "{:?}",
        end.said
    );
    assert_eq!(obs(&stage, "leader_before"), json!(true));
    assert!(notes.contains("shows no forward-security line"), "{notes}");
    finish(&stage, run);
}

#[test]
fn flight_mode_no_lead_in_its_time_is_a_fail_with_no_halt_and_no_text_asked() {
    let (stage, run) = online_stage("t7-flight-none");
    let timing = StepTiming {
        d1_wait: Duration::from_millis(400),
        ..short()
    };
    let end = drive(
        &stage,
        "flight-mode",
        &[],
        timing,
        &[("flight-on", true)],
        |_, _| {},
    );
    assert_eq!(end.status, 1);
    assert!(end.reason.contains("leader not observed"), "{}", end.reason);
    assert!(end.reason.contains("flight mode OFF"), "{}", end.reason);
    assert!(
        !end.said.iter().any(|l| l.contains("send ONE short text")),
        "{:?}",
        end.said
    );
    assert_eq!(obs(&stage, "promoted"), json!(false));
    assert!(run.info().expect("info")["halted"].is_null());
    finish(&stage, run);
}

#[test]
fn flight_mode_waits_for_the_lead_across_calls_by_the_clock_of_flight_mode() {
    let (stage, run) = online_stage("t7-flight-resume");
    let timing = StepTiming {
        d1_wait: Duration::from_secs(30),
        call_max: Duration::from_millis(600),
        ..short()
    };
    let first = drive(
        &stage,
        "flight-mode",
        &[],
        timing,
        &[("flight-on", true)],
        |_, _| {},
    );
    assert_eq!(first.status, 1);
    assert!(
        first.reason.contains("run `step flight-mode` again"),
        "{}",
        first.reason
    );
    let record = stage.records().pop().expect("a record");
    assert!(record["resume"]["on_ms"].is_u64(), "{record}");
    // The lead comes between the calls; the second call asks no "flight-on" again and goes on.
    stage.cues.line(D1_LINE);
    let answers = [
        ("pp-delivered", true),
        ("flight-off-both", true),
        ("tp-shows-later", true),
        ("fs-line", true),
        ("phone-warning", false),
    ];
    let no_count = StepTiming {
        count_wait: Duration::from_millis(300),
        ..short()
    };
    let second = drive(
        &stage,
        "flight-mode",
        &[],
        no_count,
        &answers,
        |cues, hear| {
            hear.until("(the TEST phone is offline)");
            two_chats(cues, (1, 2000), (0, 900));
            hear.until("the TEST phone is still offline");
            two_chats(cues, (1, 2000), (1, 1900));
            hear.until("ONE more short text");
            two_chats(cues, (2, 3000), (1, 1900));
        },
    );
    assert_eq!(second.status, 0, "{}", second.reason);
    assert!(
        !second.said.iter().any(|l| l.contains("FLIGHT MODE on")),
        "{:?}",
        second.said
    );
    assert_eq!(obs(&stage, "promoted"), json!(true));
    finish(&stage, run);
}

#[test]
fn flight_mode_s7_two_texts_where_one_was_sent_and_a_text_that_never_comes_halt() {
    // Two new messages in the 1:1 chat where one was sent.
    let (stage, run) = online_stage("t7-flight-twice");
    let end = drive(
        &stage,
        "flight-mode",
        &[],
        short(),
        &FLIGHT_ANSWERS,
        |cues, hear| {
            hear.until("WAIT:");
            cues.line(D1_LINE);
            hear.until("(the TEST phone is offline)");
            two_chats(cues, (2, 2000), (0, 900));
        },
    );
    assert_eq!(end.status, 1);
    assert!(end.reason.contains("TWICE"), "{}", end.reason);
    finish_halted(run, "twice", &stage);

    // The group text never comes within the bound: counted lost.
    let (stage, run) = online_stage("t7-flight-lost");
    let timing = StepTiming {
        lost_after: Duration::from_millis(600),
        ..short()
    };
    let end = drive(
        &stage,
        "flight-mode",
        &[],
        timing,
        &FLIGHT_ANSWERS,
        |cues, hear| {
            hear.until("WAIT:");
            cues.line(D1_LINE);
            hear.until("(the TEST phone is offline)");
            two_chats(cues, (1, 2000), (0, 900));
        },
    );
    assert_eq!(end.status, 1);
    assert!(end.reason.contains("counted LOST"), "{}", end.reason);
    finish_halted(run, "lost", &stage);
}

#[test]
fn flight_mode_s7_s9_and_the_findings_by_his_answers() {
    // Flight mode off and the phone lacks a text: S7, the run halts.
    let (stage, run) = online_stage("t7-flight-off-no");
    let mut answers = FLIGHT_ANSWERS;
    answers[2] = ("flight-off-both", false);
    let end = drive(
        &stage,
        "flight-mode",
        &[],
        short(),
        &answers,
        flight_play(2),
    );
    assert_eq!(end.status, 1);
    assert!(end.reason.contains("halted (lost)"), "{}", end.reason);
    finish_halted(run, "lost", &stage);

    // A warning on a phone: S9 (a), the run halts.
    let (stage, run) = online_stage("t7-flight-s9");
    let mut answers = FLIGHT_ANSWERS;
    answers[5] = ("phone-warning", true);
    let end = drive(
        &stage,
        "flight-mode",
        &[],
        short(),
        &answers,
        flight_play(3),
    );
    assert_eq!(end.status, 1);
    assert!(end.reason.contains("phone-warning"), "{}", end.reason);
    // The forward-security line, question (b), was asked and recorded before: no stop of its own.
    assert_eq!(obs(&stage, "fs_line"), json!(true));
    finish_halted(run, "phone-warning", &stage);

    // The personal phone does not show the 1:1 text as delivered: a finding, not a halt.
    let (stage, run) = online_stage("t7-flight-undelivered");
    let mut answers = FLIGHT_ANSWERS;
    answers[1] = ("pp-delivered", false);
    let end = drive(
        &stage,
        "flight-mode",
        &[],
        short(),
        &answers,
        flight_play(3),
    );
    assert_eq!(end.status, 1);
    assert!(
        end.reason
            .contains("did not show the 1:1 text as delivered"),
        "{}",
        end.reason
    );
    assert!(run.info().expect("info")["halted"].is_null());
    finish(&stage, run);
}

#[test]
fn flight_mode_reads_a_halt_and_says_so() {
    let (stage, run) = online_stage("t7-flight-halt");
    let end = drive(
        &stage,
        "flight-mode",
        &[],
        short(),
        &FLIGHT_ANSWERS,
        |cues, hear| {
            hear.until("WAIT:");
            cues.line(ERROR_LINE);
        },
    );
    assert_eq!(end.status, 1);
    assert!(
        end.reason.contains("the run is halted (error)"),
        "{}",
        end.reason
    );
    finish_halted(run, "error", &stage);
}

// ---- removed-on-phone and unlink --------------------------------------------------------------------------

#[test]
fn removed_on_phone_tells_the_judge_first_then_the_4113_and_the_logged_out_account_are_no_halt() {
    let (stage, run) = online_stage("t7-removed");
    assert_eq!(run.info().expect("info")["removal"], json!(false));
    let end = drive(
        &stage,
        "removed-on-phone",
        &[],
        short(),
        &[("end-go", true)],
        |cues, hear| {
            hear.until("REMOVE the entry");
            cues.line(E1_4113);
            cues.event(&account_event("logged_out", "logged_out"));
        },
    );
    assert_eq!(end.status, 0, "{}", end.reason);
    let notes = notes_of(&end);
    assert!(
        notes.contains("close 4113 seen: yes; account logged out: yes"),
        "{notes}"
    );
    assert!(
        end.said
            .iter()
            .any(|l| l.contains("wipe --yes-delete-the-login")),
        "{:?}",
        end.said
    );
    // Told BEFORE the phone was touched: the 4113 and `logged_out` did not halt the run.
    let info = run.info().expect("info");
    assert_eq!(info["removal"], json!(true));
    assert!(info["halted"].is_null(), "{info}");
    assert_eq!(obs(&stage, "close_4113"), json!(true));
    assert_eq!(obs(&stage, "logged_out"), json!(true));
    read_only(&stage);
    nothing_of_a_value(&end, &stage);
    finish(&stage, run);
}

#[test]
fn the_ends_ask_once_more_and_a_no_changes_nothing_and_a_close_that_never_comes_is_a_fail() {
    for name in ["removed-on-phone", "unlink"] {
        let (stage, run) = online_stage(&format!("t7-end-no-{name}"));
        let end = drive(&stage, name, &[], short(), &[("end-go", false)], |_, _| {});
        assert_eq!(end.status, 1, "{name}");
        assert!(end.reason.contains("do not want"), "{name}: {}", end.reason);
        assert_eq!(run.info().expect("info")["removal"], json!(false), "{name}");
        assert!(
            !end.said
                .iter()
                .any(|l| l.starts_with("PHONE:") || l.starts_with("WINDOW:")),
            "{name}: {:?}",
            end.said
        );
        finish(&stage, run);
    }
    // The phone removed nothing in the time: a FAIL with the words, and the removal is GIVEN BACK to
    // the judge (LOW-9): a 4113 that comes after the failed step halts the run.
    let (stage, run) = online_stage("t7-removed-never");
    let timing = StepTiming {
        removal_wait: Duration::from_millis(400),
        ..short()
    };
    let end = drive(
        &stage,
        "removed-on-phone",
        &[],
        timing,
        &[("end-go", true)],
        |_, _| {},
    );
    assert_eq!(end.status, 1);
    assert!(end.reason.contains("did not show in"), "{}", end.reason);
    assert!(
        notes_of(&end).contains("close 4113 seen: NO"),
        "{}",
        notes_of(&end)
    );
    assert_eq!(
        run.info().expect("info")["removal"],
        json!(false),
        "a failed end step gives the removal back"
    );
    assert_eq!(obs(&stage, "removal_given_back"), json!(true));
    stage.cues.line(E1_4113);
    let mut run = run;
    run.wait_for("the 4113 halts again", |i| {
        i["halted"] == json!("close-4113")
    });
    finish_halted(run, "close-4113", &stage);
}

/// LOW-9, `unlink`: the same. A failure (the drop was not reached, the account did not go) gives the
/// removal back; a step that PASSES keeps it asked for the rest of the run (its own end).
#[test]
fn a_failed_unlink_gives_the_removal_back_and_a_passed_one_keeps_it() {
    let (stage, run) = online_stage("t7-unlink-back");
    let timing = StepTiming {
        removal_wait: Duration::from_millis(300),
        ..short()
    };
    let end = drive(
        &stage,
        "unlink",
        &[],
        timing,
        &[("end-go", true)],
        |_, _| {},
    );
    assert_eq!(end.status, 1);
    assert!(end.reason.contains("did not finish"), "{}", end.reason);
    assert_eq!(run.info().expect("info")["removal"], json!(false));
    let events = run
        .call(&json!({"op": "events", "since": 0}))
        .expect("events");
    let names: Vec<&str> = events["events"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|e| e["event"].as_str())
        .collect();
    let (asked, over) = (
        names.iter().position(|n| *n == "removal_asked"),
        names.iter().position(|n| *n == "removal_over"),
    );
    assert!(asked < over && over.is_some(), "{names:?}");
    finish(&stage, run);
}

#[test]
fn unlink_drops_the_device_the_account_goes_and_the_phones_list_is_clean() {
    let stage = Stage::new("t7-unlink");
    stage.linked();
    stage.cues.chats(&[chat(1, "direct", 0, 1000, "none")]);
    stage.cues.put(
        "state.json",
        &json!({"open": 0, "rows": [], "accounts": ["online"]}),
    );
    let mut run = stage.start(false);
    online(&stage, &mut run);
    let answers = [("end-go", true), ("tp-list-clean", true)];
    let end = drive(&stage, "unlink", &[], short(), &answers, |cues, hear| {
        hear.until("Remove, and confirm");
        // The socket's end line with the drop frame in it: without the removal told to the judge
        // this is a follower that wrote, and the run would halt.
        cues.line(&count_line(&[("when", "end"), ("out_drop", "1")]));
        cues.line(D4_ACKED);
        cues.put(
            "state.json",
            &json!({"open": 0, "rows": [], "accounts": []}),
        );
    });
    assert_eq!(end.status, 0, "{}", end.reason);
    let notes = notes_of(&end);
    assert!(
        notes.contains("the device's drop on the mediator: Removed"),
        "{notes}"
    );
    assert!(
        notes.contains("the account is gone from the window: yes"),
        "{notes}"
    );
    assert!(
        notes.contains("linked devices list is clean: yes"),
        "{notes}"
    );
    assert_eq!(obs(&stage, "drop"), json!("removed"));
    assert_eq!(obs(&stage, "account_gone"), json!(true));
    assert_eq!(obs(&stage, "d4_acked"), json!(1));
    assert_eq!(obs(&stage, "list_clean"), json!(true));
    let info = run.info().expect("info");
    assert_eq!(info["removal"], json!(true));
    assert!(info["halted"].is_null(), "{info}");
    read_only(&stage);
    nothing_of_a_value(&end, &stage);
    finish(&stage, run);
}

#[test]
fn unlink_a_drop_that_did_not_reach_and_a_list_that_is_not_clean_are_findings() {
    // The drop did not reach the mediator: the words send him to the phone's list.
    let stage = Stage::new("t7-unlink-nr");
    stage.linked();
    stage.cues.chats(&[chat(1, "direct", 0, 1000, "none")]);
    stage.cues.put(
        "state.json",
        &json!({"open": 0, "rows": [], "accounts": ["online"]}),
    );
    let mut run = stage.start(false);
    online(&stage, &mut run);
    let end = drive(
        &stage,
        "unlink",
        &[],
        short(),
        &[("end-go", true)],
        |cues, hear| {
            hear.until("Remove, and confirm");
            cues.line(D4_NOT_REACHED);
            cues.put(
                "state.json",
                &json!({"open": 0, "rows": [], "accounts": []}),
            );
        },
    );
    assert_eq!(end.status, 1);
    assert!(
        end.reason.contains("did not finish as expected"),
        "{}",
        end.reason
    );
    assert!(end.reason.contains("by hand"), "{}", end.reason);
    assert!(notes_of(&end).contains("NOT reached"), "{}", notes_of(&end));
    assert!(run.info().expect("info")["halted"].is_null());
    finish(&stage, run);

    // The drop was acked but the phone's list still shows the entry.
    let stage = Stage::new("t7-unlink-dirty");
    stage.linked();
    stage.cues.chats(&[chat(1, "direct", 0, 1000, "none")]);
    stage.cues.put(
        "state.json",
        &json!({"open": 0, "rows": [], "accounts": ["online"]}),
    );
    let mut run = stage.start(false);
    online(&stage, &mut run);
    let answers = [("end-go", true), ("tp-list-clean", false)];
    let end = drive(&stage, "unlink", &[], short(), &answers, |cues, hear| {
        hear.until("Remove, and confirm");
        cues.line(D4_ACKED);
        cues.put(
            "state.json",
            &json!({"open": 0, "rows": [], "accounts": []}),
        );
    });
    assert_eq!(end.status, 1);
    assert!(
        end.reason.contains("still shows a Kourier entry"),
        "{}",
        end.reason
    );
    assert!(
        notes_of(&end).contains("list is clean: NO"),
        "{}",
        notes_of(&end)
    );
    finish(&stage, run);
}

#[test]
fn the_removal_op_takes_two_words_and_no_removal_in_a_halted_run() {
    let (stage, run) = online_stage("t7-removal-op");
    // Neither an unknown word nor an empty one sets it.
    for what in ["nonsense", ""] {
        let e = run
            .call(&json!({"op": "removal", "what": what}))
            .expect_err("a refusal");
        assert!(format!("{e:#}").contains("badRequest"), "{e:#}");
    }
    assert_eq!(run.info().expect("info")["removal"], json!(false));
    // The two words set it, and it shows in `info` and in the events.
    run.call(&json!({"op": "removal", "what": "unlink"}))
        .expect("asked");
    assert_eq!(run.info().expect("info")["removal"], json!(true));
    let events = run
        .call(&json!({"op": "events", "since": 0}))
        .expect("events");
    assert!(
        events["events"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|e| e["event"] == "removal_asked" && e["data"]["what"] == "unlink"),
        "{events}"
    );
    finish(&stage, run);

    // A halted run takes none: it is ending.
    let (stage, run) = online_stage("t7-removal-halted");
    stage.cues.line(ERROR_LINE);
    let mut run = run;
    run.wait_for("the halt", |i| i["halted"].is_string());
    let e = run
        .call(&json!({"op": "removal", "what": "removed-on-phone"}))
        .expect_err("a refusal");
    assert!(format!("{e:#}").contains("halted"), "{e:#}");
    assert_eq!(run.info().expect("info")["removal"], json!(false));
    finish_halted(run, "error", &stage);
}
