//! `report` (T7): the file the profile's records, state and `app.log` make: the header, the steps, the
//! sockets, the lines, the leader observations, THE [U] TABLE and what stays open, and that no ID, no
//! digest and no cell-breaking `|` reaches it. The profile is a scratch directory with hand-made
//! records; nothing runs, nothing contacts a host.

use serde_json::{Value, json};

use super::supervise::Rig;
use super::{digest_shape, id_shape, mode_of};
use crate::threema_run::profile::{State, append_line};
use crate::threema_run::report::{AppLog, report_in, report_text};
use crate::threema_run::steps::STEPS;

fn record(name: &str, result: &str, reason: &str, notes: &[&str], facts: Value) -> Value {
    json!({
        "name": name, "result": result, "reason": reason,
        "started_ms": 1_762_250_000_000_u64, "ended_ms": 1_762_250_042_000_u64,
        "notes": notes, "resume": null, "facts": facts,
    })
}

fn write_records(rig: &Rig, records: &[Value]) {
    for r in records {
        append_line(&rig.dir.steps_log(), &r.to_string()).expect("a record");
    }
}

/// A whole count line (the contract's field order), with the counters that matter.
fn count(when: &str, in_csp: u64, unblock: u64, boxes: u64) -> String {
    format!(
        "2026-11-04T10:00:00.000000Z  INFO kourier_threema::conn::lines: socket frame counts when={when} in_reflected=3 in_reflect_ack=0 in_dry=1 in_promoted=0 in_drop_ack=0 in_tx=0 in_csp_message={in_csp} in_message_ack=0 in_queue_complete=1 in_echo=2 in_close_error=0 in_other=0 out_reflected_ack=3 out_reflect_job={r} out_reflect_transmit=0 out_reflect_inbound={in_csp} out_ephemeral=0 out_box_job={boxes} out_box_transmit=0 out_message_ack={in_csp} out_unblock={unblock} out_tx=0 out_echo=2 out_idle=1 out_drop=0 out_other=0 held=0 held_by=none",
        r = boxes * 2
    )
}

const LINK_NOTES: [&str; 5] = [
    "allow list: taken by the start, removed at its first answer",
    "emoji: he said the three were equal",
    "the TEST phone's linked devices list shows the Kourier entry: yes",
    "the menu path on the TEST phone was the runbook's: yes",
    "the login reached done 12 s into the step",
];

fn app_log() -> String {
    [
        "2026-11-04T10:00:00.000000Z  INFO kourier_core: something that is no contract line".to_owned(),
        count("running", 0, 0, 0),
        count("end", 0, 0, 0),
        "2026-11-04T10:05:00.000000Z  INFO kourier_threema::conn: incoming messages were unblocked as the leader"
            .to_owned(),
        count("running", 2, 1, 2),
        "2026-11-04T10:06:02.000000Z  INFO kourier_threema::conn: the mediator closed the connection code=1000 meaning=\"Normal closure\""
            .to_owned(),
        count("end", 3, 1, 3),
        count("running", 0, 0, 0),
    ]
    .join("\n")
}

fn rig_with_a_run(name: &str) -> Rig {
    let rig = Rig::new(name);
    let mut state = State::load(&rig.dir).expect("state").expect("a state file");
    state.set("linked_since_ms", json!(1_762_250_000_000_u64));
    state.set("last_online_ms", json!(1_762_259_000_000_u64));
    state.set("starts", json!(2));
    state.set("core_schema", json!(21));
    state.set("build_commit", json!("abcdef0123456789abcdef"));
    state.save(&rig.dir).expect("saved");
    std::fs::write(rig.dir.app_log(), app_log()).expect("app.log");
    write_records(
        &rig,
        &[
            record("link", "PASS", "", &LINK_NOTES, json!({"linked": true})),
            record(
                "sync",
                "PASS",
                "",
                &[
                    "online 5 s after the login was done",
                    "first count line: when=running in_reflected=3 in_csp_message=0",
                    "no leader promotion was seen (the phone leads)",
                ],
                json!({"chats": []}),
            ),
            record(
                "receive",
                "PASS",
                "",
                &["one new message, once, in direct conversation #1"],
                json!(null),
            ),
            record(
                "kinds",
                "PASS",
                "",
                &[
                    "new messages: 2 in 1:1 chats, 1 in groups; chats whose activity moved: 1 1:1, 1 group (counts only, no verdict per kind)",
                ],
                json!({"obs": {"new_direct": 2, "new_group": 1}}),
            ),
            record(
                "phone-sleep",
                "PASS",
                "",
                &[
                    "leader observed in sleep: NO (no D1 in 600 s): the phone kept the lead",
                    "push mode (typed by the user): Threema's own push service",
                ],
                json!({"obs": {"push_own": true, "push_origin": "typed by the user", "leader_in_sleep": false, "leader_at_start": false}}),
            ),
            record(
                "flight-mode",
                "FAIL",
                // A reason that would break a table cell and holds the shapes of an ID and a digest.
                &format!(
                    "leader not observed | {} and {}",
                    id_shape(),
                    digest_shape()
                ),
                &[format!("a note with the shape of an ID: {}", id_shape()).as_str()],
                json!({"obs": {"promoted": false, "leader_before": false, "d1_after_on_s": null}}),
            ),
        ],
    );
    rig
}

fn report_of(rig: &Rig) -> String {
    let lines = report_in(&rig.dir, 1_762_260_000_000).expect("the report");
    assert_eq!(lines.len(), 1);
    std::fs::read_to_string(rig.dir.report_file()).expect("report.md")
}

#[test]
fn the_report_holds_the_header_the_steps_the_sockets_the_u_table_and_what_stays_open() {
    let rig = rig_with_a_run("t7-report");
    let text = report_of(&rig);
    // The header.
    assert!(text.starts_with("# Kourier Threema run"), "{text}");
    assert!(
        text.contains("- Build: abcdef0123 (core.db schema 21)"),
        "{text}"
    );
    assert!(text.contains("- Login: linked since 2025-11-04"), "{text}");
    assert!(text.contains("- Starts: 2"), "{text}");
    assert!(text.contains("- Link calls: 1"), "{text}");
    assert!(text.contains("- Halted: no"), "{text}");
    assert!(
        text.contains("- Steps: 6 calls, 5 passed, 1 failed"),
        "{text}"
    );
    // The steps in order, and the latest result of each step of the table (a step that did not run
    // says so).
    assert!(text.contains("| 1 | link | PASS |"), "{text}");
    assert!(text.contains("| 6 | flight-mode | FAIL |"), "{text}");
    for def in STEPS {
        assert!(text.contains(&format!("| {} | ", def.name)), "{}", def.name);
    }
    assert!(text.contains("| unlink | not run | 0 |"), "{text}");
    assert!(text.contains("| flight-mode | FAIL | 1 |"), "{text}");
    // What each step saw.
    assert!(text.contains("- allow list: taken by the start"), "{text}");
    // The sockets: two ended sockets with their counters, the last running line with no end.
    let sockets = text.split("## The sockets").nth(1).expect("the sockets");
    let sockets = sockets.split("\n## ").next().expect("the section");
    assert!(
        sockets.contains("| 1 | 2026-11-04T10:00:00.000000Z | ended |"),
        "{sockets}"
    );
    assert!(
        sockets.contains("| 2 | 2026-11-04T10:00:00.000000Z | ended |"),
        "{sockets}"
    );
    assert!(
        sockets.contains("| 3 | 2026-11-04T10:00:00.000000Z | running, no end line"),
        "{sockets}"
    );
    assert!(
        sockets.contains("| in csp |") || sockets.contains(" in csp |"),
        "{sockets}"
    );
    assert!(
        sockets.contains("5 count lines in all (3 running)"),
        "{sockets}"
    );
    // The lines of the contract, the closes and the lead's time.
    assert!(text.contains("| E1_CLOSED | 1 |"), "{text}");
    assert!(
        text.contains("The mediator's closes (E1): 1000 x1."),
        "{text}"
    );
    assert!(
        text.contains("Kourier took the lead (D1) at: 2026-11-04T10:05:00.000000Z."),
        "{text}"
    );
    // The leader observations and the push mode.
    assert!(
        text.contains(
            "Push mode on the TEST phone (typed by the user): Threema's own push service."
        ),
        "{text}"
    );
    assert!(text.contains("took the lead in the sleep: no"), "{text}");
    assert!(text.contains("`reconnect`: not run."), "{text}");
    // THE [U] TABLE: settled where the step passed, finding where it failed, not run otherwise.
    let row = |needle: &str| {
        text.lines()
            .find(|l| l.contains(needle))
            .unwrap_or_else(|| panic!("no row for {needle}: {text}"))
            .to_owned()
    };
    assert!(row("The list of test IDs was taken in memory").contains("| settled |"));
    assert!(row("The list of test IDs was taken in memory").contains("E6 lines: 0"));
    assert!(row("The label 'Kourier (<host>)'").contains("| settled |"));
    assert!(row("A quote, a mention, bold text").contains("| settled |"));
    assert!(row("The leader rule while the TEST phone sleeps").contains("| settled |"));
    assert!(row("The leader receive: the promotion's delay").contains("| finding |"));
    assert!(row("END A: the 4113 path").contains("| not run |"));
    assert!(row("END B: RP-b's drop").contains("| not run |"));
    assert!(row("No stop rule fired").contains("| settled |"));
    // The one-to-one receive passed, the group one never ran: not settled as a pair.
    assert!(row("One incoming text once in the 1:1 chat").contains("| not run |"));
    assert!(text.contains("## What stays [U] after the run"), "{text}");
    assert!(text.contains("TH6a's send list beyond receipts"), "{text}");
}

#[test]
fn nothing_of_an_id_a_digest_or_a_cell_breaking_bar_reaches_the_report() {
    let rig = rig_with_a_run("t7-report-redact");
    let text = report_of(&rig);
    assert!(!text.contains(&id_shape()), "{text}");
    assert!(!text.contains(&digest_shape()), "{text}");
    assert!(text.contains("[id]") && text.contains("[digest]"), "{text}");
    // The reason's bar became a slash, so the steps table keeps its six cells on that row.
    let row = text
        .lines()
        .find(|l| l.starts_with("| 6 | flight-mode | FAIL |"))
        .expect("the row");
    assert!(row.contains("leader not observed / "), "{row}");
    assert_eq!(row.matches('|').count(), 7, "{row}");
}

#[test]
fn a_halted_profile_names_the_kind_and_the_drop_and_a_missing_log_is_not_measured() {
    let rig = Rig::new("t7-report-halt");
    let mut state = State::load(&rig.dir).expect("state").expect("a state file");
    state.set(
        "halted",
        json!({"kind": "link-failed-after-hello", "at_ms": 1_762_250_000_000_u64, "drop": "removed"}),
    );
    state.save(&rig.dir).expect("saved");
    let text = report_of(&rig);
    assert!(
        text.contains("- Halted: link-failed-after-hello at 2025-11-04"),
        "{text}"
    );
    assert!(text.contains("(the device's drop: removed)"), "{text}");
    assert!(text.contains("- Login: not linked"), "{text}");
    assert!(
        text.contains("No count line is in `app.log`: the frame counts were not measured."),
        "{text}"
    );
    let last = text
        .lines()
        .find(|l| l.contains("No stop rule fired"))
        .expect("the row");
    assert!(last.contains("| finding |"), "{last}");
    assert!(last.contains("halted: link-failed-after-hello"), "{last}");
    // No step ran: every question of the table stays open.
    assert!(text.contains("| link | not run | 0 |"), "{text}");
}

#[test]
fn report_in_writes_a_private_file_and_says_how_many_calls_it_holds() {
    let rig = rig_with_a_run("t7-report-file");
    let lines = report_in(&rig.dir, 1_762_260_000_000).expect("the report");
    assert_eq!(
        lines,
        [format!(
            "report:   {} (6 step calls, 5 passed, 1 failed)",
            rig.dir.report_file().display()
        )]
    );
    assert_eq!(mode_of(&rig.dir.report_file()), 0o600);
    // A profile with no state file is no profile of this tool.
    let none = Rig::new("t7-report-none");
    std::fs::remove_file(none.dir.state_file()).expect("no state");
    let e = report_in(&none.dir, 1).expect_err("no report");
    assert!(format!("{e:#}").contains("no state file"), "{e:#}");
}

#[test]
fn the_text_is_a_function_of_the_records_and_the_log_alone() {
    // The same input gives the same text: the report starts nothing and asks nobody.
    let rig = rig_with_a_run("t7-report-pure");
    let state = State::load(&rig.dir).expect("state").expect("a state file");
    let steps: Vec<Value> = std::fs::read_to_string(rig.dir.steps_log())
        .expect("steps")
        .lines()
        .map(|l| serde_json::from_str(l).expect("a record"))
        .collect();
    let log = AppLog::read(&rig.dir.app_log());
    let a = report_text(&rig.dir, &state, &steps, &log, 5);
    let b = report_text(&rig.dir, &state, &steps, &log, 5);
    assert_eq!(a, b);
}

/// T-H1's step-aside (TH6 L6): the report names it, with the hold that led to it, and says it is no
/// halt; a log with none does not mention one.
#[test]
fn the_report_names_a_step_aside_and_says_it_is_no_halt() {
    let rig = Rig::new("t10-report-aside");
    let log = [
        "2026-11-04T10:05:00.000000Z  INFO kourier_threema::conn: incoming messages were unblocked as the leader",
        "2026-11-04T10:06:00.000000Z  INFO kourier_threema::conn: a chat server message is held for the phone rule=csp-unknown-type",
        "2026-11-04T10:07:00.000000Z  INFO kourier_threema::conn::lines: a server payload Kourier does not handle server=mediator kind=DevicesInfo",
        "2026-11-04T10:07:00.100000Z  INFO kourier_threema::conn: messages are held for the phone and another device is online; stepping aside so it can lead held=1",
    ]
    .join("\n");
    std::fs::write(rig.dir.app_log(), log).expect("app.log");
    let text = report_of(&rig);
    assert!(text.contains("| L_STEP_ASIDE | 1 |"), "{text}");
    assert!(
        text.contains("Kourier STEPPED ASIDE for the phone")
            && text.contains("no halt) at: 2026-11-04T10:07:00.100000Z"),
        "{text}"
    );
    assert!(text.contains("csp-unknown-type x1"), "{text}");
    assert!(text.contains("mediator/DevicesInfo x1"), "{text}");
    // A log without one says nothing of it.
    std::fs::write(rig.dir.app_log(), app_log()).expect("app.log");
    assert!(!report_of(&rig).contains("STEPPED ASIDE"));
}

/// RT1 (2026-11-05): the report's redaction masked the tool's own word "PERSONAL" as "[id]".
#[test]
fn the_tools_own_word_personal_survives_into_the_report_and_an_id_shape_from_outside_does_not_rt1()
{
    let rig = rig_with_a_run("rt1-report-personal");
    let outside = format!("a value from outside: {}", id_shape());
    write_records(
        &rig,
        &[record(
            "sync",
            "PASS",
            "",
            &[
                "the PERSONAL phone shows the 1:1 text once",
                outside.as_str(),
            ],
            json!(null),
        )],
    );
    let text = report_of(&rig);
    assert!(
        text.contains("the PERSONAL phone shows the 1:1 text once"),
        "{text}"
    );
    assert!(!text.contains("[id] phone"), "{text}");
    assert!(!text.contains(&id_shape()), "{text}");
    assert!(text.contains("a value from outside: [id]"), "{text}");
}

/// RT1: a push mode that was never looked at is "not looked at", not an answer he gave.
#[test]
fn a_push_mode_recorded_as_not_looked_at_is_reported_so_never_as_given_rt1() {
    let rig = rig_with_a_run("rt1-report-push");
    write_records(
        &rig,
        &[record(
            "phone-sleep",
            "PASS",
            "",
            &[
                "push mode: not looked at (the notification setting could not be read on the TEST phone)",
            ],
            json!({"obs": {"push_own": null, "push_origin": "not looked at"}}),
        )],
    );
    let text = report_of(&rig);
    assert!(
        text.contains("- Push mode on the TEST phone: not looked at"),
        "{text}"
    );
    assert!(!text.contains("he gave"), "{text}");
}
