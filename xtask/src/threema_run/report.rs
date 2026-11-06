//! `report` (T7; `briefs/tr.md` §2.1 and §5, wa_run.rs's `report_text`): writes
//! `threema-run/report.md` into the profile directory from the profile's files alone (the state file,
//! `steps.jsonl`, `app.log`). It starts nothing, asks the supervisor nothing and contacts no host, so it
//! also works on a stopped profile and after a halt.
//!
//! What it holds: counts, words and times, never an ID, a digest, a name or a text (every cell goes
//! through [`super::steps`]'s `safe_text`, and the whole file through [`super::supervise::redact`] as a
//! last belt). The header (profile, build, login, starts, the halt with the drop's outcome), the
//! steps in the order they ran, the latest result of each step, what each saw (its notes), THE
//! SOCKETS (every count line of `app.log` that ended a socket, with the counters the leader rule is
//! judged on, so the first real count is on record; a socket with no end line is "not measured"), the
//! lines of the contract and the closes, the leader observations and the push mode, and THE [U] TABLE
//! of `briefs/tr.md` §5 filled from the records (settled / finding / not run), then what stays [U].

use std::collections::BTreeMap;
use std::io::BufRead as _;

use anyhow::{Context, Result};
use serde_json::Value;

use super::lines::{Body, Count, parse};
use super::preflight::utc;
use super::profile::{ProfileDir, State, write_private};
use super::steps::{STEPS, recorded_steps, safe_text};
use super::supervise::redact;
use super::{Args, var};
use crate::target_dir;

/// The counters of a socket's end line that the report prints, with their column names.
const SOCKET_COLUMNS: [(&str, &str); 11] = [
    ("in_reflected", "in reflected"),
    ("in_csp_message", "in csp"),
    ("out_reflected_ack", "out reflected-ack"),
    ("out_reflect_job", "out reflect job"),
    ("out_reflect_inbound", "out reflect inbound"),
    ("out_box_job", "out box job"),
    ("out_message_ack", "out message-ack"),
    ("out_unblock", "out unblock"),
    ("out_drop", "out drop"),
    ("in_other", "in other"),
    ("held", "held"),
];

/// One count line of `app.log`.
#[derive(Debug)]
struct SocketLine {
    time: String,
    count: Count,
}

/// What the report reads of `app.log`: the count lines in order, the tally of the contract lines
/// and the closes.
#[derive(Debug, Default)]
pub(super) struct AppLog {
    counts: Vec<SocketLine>,
    rows: BTreeMap<&'static str, u64>,
    closes: BTreeMap<u16, u64>,
    held: BTreeMap<String, u64>,
    kinds: BTreeMap<String, u64>,
    d1_times: Vec<String>,
    /// The times of T-H1's step-aside lines (the leader closed 1000 so the phone could lead).
    asides: Vec<String>,
}

/// The time column of a tracing line: its first token when it has the shape of a time.
fn time_of(raw: &str) -> String {
    raw.split_whitespace()
        .next()
        .filter(|t| t.len() <= 40 && t.starts_with(|c: char| c.is_ascii_digit()) && t.contains('T'))
        .unwrap_or("?")
        .to_owned()
}

impl AppLog {
    /// Reads the lines of `app.log` (already redacted by the supervisor; bytes that are no UTF-8 are
    /// replaced). A missing file is an empty log.
    pub(super) fn read(path: &std::path::Path) -> Self {
        let Ok(file) = std::fs::File::open(path) else {
            return Self::default();
        };
        let mut log = Self::default();
        let mut reader = std::io::BufReader::new(file);
        let mut buf = Vec::new();
        while reader.read_until(b'\n', &mut buf).is_ok_and(|n| n > 0) {
            log.take(String::from_utf8_lossy(&buf).trim_end_matches(['\n', '\r']));
            buf.clear();
        }
        log
    }

    pub(super) fn take(&mut self, raw: &str) {
        let Some(parsed) = parse(raw) else {
            return;
        };
        *self.rows.entry(parsed.row).or_insert(0) += 1;
        match parsed.body {
            Body::Count(count) => self.counts.push(SocketLine {
                time: time_of(raw),
                count,
            }),
            Body::Closed { code } => *self.closes.entry(code).or_insert(0) += 1,
            Body::Held { rule } => *self.held.entry(rule).or_insert(0) += 1,
            Body::Kind { server, kind } => {
                *self.kinds.entry(format!("{server}/{kind}")).or_insert(0) += 1;
            }
            Body::Aside { .. } => self.asides.push(time_of(raw)),
            _ => {
                if parsed.row == "D1_UNBLOCKED" {
                    self.d1_times.push(time_of(raw));
                }
            }
        }
    }

    fn rows_of(&self, row: &str) -> u64 {
        self.rows.get(row).copied().unwrap_or(0)
    }
}

// ---- Reading the records -----------------------------------------------------------------------------------

/// The latest record of the step `name`.
fn latest<'a>(steps: &'a [Value], name: &str) -> Option<&'a Value> {
    steps.iter().rev().find(|s| s["name"] == name)
}

/// The latest PASS of the step `name`, else its latest record: where a note is looked for.
fn best<'a>(steps: &'a [Value], name: &str) -> Option<&'a Value> {
    steps
        .iter()
        .rev()
        .find(|s| s["name"] == name && s["result"] == "PASS")
        .or_else(|| latest(steps, name))
}

/// The first note of the step `name` that starts with `prefix`.
fn note_of(steps: &[Value], name: &str, prefix: &str) -> Option<String> {
    best(steps, name)?["notes"]
        .as_array()?
        .iter()
        .filter_map(Value::as_str)
        .find(|n| n.starts_with(prefix))
        .map(str::to_owned)
}

/// What a step recorded under `facts.obs.<key>` (the latest record that has it).
fn obs_of<'a>(steps: &'a [Value], name: &str, key: &str) -> Option<&'a Value> {
    steps
        .iter()
        .rev()
        .filter(|s| s["name"] == name)
        .map(|s| &s["facts"]["obs"][key])
        .find(|v| !v.is_null())
}

/// `settled` when every step in `names` ended PASS (its latest record), `finding` when one ended FAIL,
/// else `not run`.
fn verdict(steps: &[Value], names: &[&str]) -> &'static str {
    let results: Vec<Option<&str>> = names
        .iter()
        .map(|n| latest(steps, n).and_then(|s| s["result"].as_str()))
        .collect();
    if results.contains(&Some("FAIL")) {
        "finding"
    } else if results.iter().all(|r| *r == Some("PASS")) {
        "settled"
    } else {
        "not run"
    }
}

fn took(step: &Value) -> String {
    let (a, b) = (
        step["started_ms"].as_u64().unwrap_or(0),
        step["ended_ms"].as_u64().unwrap_or(0),
    );
    format!("{} s", b.saturating_sub(a) / 1000)
}

fn cell(text: &str) -> String {
    safe_text(text).replace('`', "'")
}

// ---- The text ---------------------------------------------------------------------------------------------

/// The steps in the order they ran, the latest result of each, and what each saw.
fn steps_sections(steps: &[Value]) -> String {
    let mut out = String::from(
        "\n## Steps in the order they ran\n\n| # | Step | Result | Started | Took | Reason |\n|---|---|---|---|---|---|\n",
    );
    for (i, s) in steps.iter().enumerate() {
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} |\n",
            i + 1,
            cell(s["name"].as_str().unwrap_or("?")),
            cell(s["result"].as_str().unwrap_or("?")),
            utc(s["started_ms"].as_u64().unwrap_or(0)),
            took(s),
            cell(s["reason"].as_str().unwrap_or("")),
        ));
    }
    out.push_str("\n## Latest result of each step\n\n| Step | Result | Calls |\n|---|---|---|\n");
    for def in STEPS {
        let calls = steps.iter().filter(|s| s["name"] == def.name).count();
        let result =
            latest(steps, def.name).map_or("not run", |s| s["result"].as_str().unwrap_or("?"));
        out.push_str(&format!("| {} | {} | {calls} |\n", def.name, cell(result)));
    }
    out.push_str("\n## What each step saw\n");
    for (i, s) in steps.iter().enumerate() {
        out.push_str(&format!(
            "\n### {}. {} ({})\n\n",
            i + 1,
            cell(s["name"].as_str().unwrap_or("?")),
            cell(s["result"].as_str().unwrap_or("?"))
        ));
        for note in s["notes"].as_array().into_iter().flatten() {
            out.push_str(&format!("- {}\n", cell(note.as_str().unwrap_or(""))));
        }
    }
    out
}

/// THE SOCKETS: every count line that ended a socket, and the last running line of a socket whose end
/// was not logged (a killed process leaves none: "not measured", never a pass).
fn sockets_section(log: &AppLog) -> String {
    let mut out = String::from("\n## The sockets (the app's frame counts)\n\n");
    let mut rows: Vec<(String, &str, &Count)> = Vec::new();
    let mut running: Option<&SocketLine> = None;
    for line in &log.counts {
        if line.count.end {
            rows.push((line.time.clone(), "ended", &line.count));
            running = None;
        } else {
            running = Some(line);
        }
    }
    if let Some(line) = running {
        rows.push((
            line.time.clone(),
            "running, no end line (not measured to its end)",
            &line.count,
        ));
    }
    if rows.is_empty() {
        out.push_str("No count line is in `app.log`: the frame counts were not measured.\n");
        return out;
    }
    out.push_str("| # | Time | State |");
    for (_, column) in SOCKET_COLUMNS {
        out.push_str(&format!(" {column} |"));
    }
    out.push_str(" held by |\n|---|---|---|");
    for _ in SOCKET_COLUMNS {
        out.push_str("---|");
    }
    out.push_str("---|\n");
    for (i, (time, state, count)) in rows.iter().enumerate() {
        out.push_str(&format!("| {} | {} | {state} |", i + 1, cell(time)));
        for (field, _) in SOCKET_COLUMNS {
            out.push_str(&format!(" {} |", count.get(field)));
        }
        let held_by = if count.held_by.is_empty() {
            "none".to_owned()
        } else {
            count
                .held_by
                .iter()
                .map(|(rule, n)| format!("{rule}:{n}"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        out.push_str(&format!(" {} |\n", cell(&held_by)));
    }
    out.push_str(&format!(
        "\n{} count lines in all ({} running). Each counter counts from its socket's open; a running line is a partial read and is never added to another.\n",
        log.counts.len(),
        log.counts.iter().filter(|l| !l.count.end).count()
    ));
    out
}

/// The tally of the contract lines, the closes, the holds and the kinds.
fn lines_section(log: &AppLog) -> String {
    let mut out = String::from("\n## The lines of the contract\n\n| Line | Times |\n|---|---|\n");
    for (row, n) in &log.rows {
        out.push_str(&format!("| {} | {n} |\n", cell(row)));
    }
    if log.rows.is_empty() {
        out.push_str("| (none) | 0 |\n");
    }
    let closes = if log.closes.is_empty() {
        "none".to_owned()
    } else {
        log.closes
            .iter()
            .map(|(code, n)| format!("{code} x{n}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    out.push_str(&format!("\nThe mediator's closes (E1): {closes}.\n"));
    if !log.held.is_empty() {
        out.push_str(&format!(
            "Held messages by rule (D2): {}.\n",
            log.held
                .iter()
                .map(|(rule, n)| format!("{rule} x{n}"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !log.kinds.is_empty() {
        out.push_str(&format!(
            "Payload kinds Kourier does not handle (L-KIND): {}.\n",
            log.kinds
                .iter()
                .map(|(kind, n)| format!("{kind} x{n}"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !log.asides.is_empty() {
        out.push_str(&format!(
            "Kourier STEPPED ASIDE for the phone (T-H1: it held a message only the phone resolves and another device was online, so it closed its socket 1000 once; no halt) at: {}.\n",
            log.asides
                .iter()
                .map(|t| cell(t))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !log.d1_times.is_empty() {
        out.push_str(&format!(
            "Kourier took the lead (D1) at: {}.\n",
            log.d1_times
                .iter()
                .map(|t| cell(t))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    out
}

/// The leader observations and the push mode, from the steps' records.
fn leader_section(steps: &[Value]) -> String {
    let mut out = String::from("\n## The leader observations and the push mode\n\n");
    let word = |v: Option<&Value>| match v {
        Some(Value::Bool(true)) => "yes".to_owned(),
        Some(Value::Bool(false)) => "no".to_owned(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::String(s)) => cell(s),
        _ => "not recorded".to_owned(),
    };
    let push_rec = steps
        .iter()
        .rev()
        .filter(|s| s["name"] == "phone-sleep")
        .map(|s| &s["facts"]["obs"])
        .find(|o| o.get("push_own").is_some());
    let push_origin = push_rec.and_then(|o| o["push_origin"].as_str()).map(cell);
    out.push_str(&match push_rec.map(|o| &o["push_own"]) {
        None => "- Push mode on the TEST phone: not recorded (`phone-sleep` did not get that far).\n".to_owned(),
        Some(Value::Null) => {
            "- Push mode on the TEST phone: not looked at (the setting could not be read and nobody was asked).\n"
                .to_owned()
        }
        Some(v) => format!(
            "- Push mode on the TEST phone ({}): {}.\n",
            push_origin.unwrap_or_else(|| "origin not recorded".to_owned()),
            if v == &Value::Bool(true) {
                "Threema's own push service"
            } else {
                "the system's push"
            }
        ),
    });
    out.push_str(&format!(
        "- `phone-sleep`: Kourier led before the step: {}; took the lead in the sleep: {}; D1 {} s after the lock.\n",
        word(obs_of(steps, "phone-sleep", "leader_at_start")),
        word(obs_of(steps, "phone-sleep", "leader_in_sleep")),
        word(obs_of(steps, "phone-sleep", "d1_after_lock_s")),
    ));
    out.push_str(&format!(
        "- `flight-mode`: Kourier led before flight mode: {}; took the lead {} s after it went on; led at the end: {}; the socket ended during the step: {}.\n",
        word(obs_of(steps, "flight-mode", "leader_before")),
        word(obs_of(steps, "flight-mode", "d1_after_on_s")),
        word(obs_of(steps, "flight-mode", "leader_at_end")),
        word(obs_of(steps, "flight-mode", "socket_ended")),
    ));
    out.push_str(&format!(
        "- `reconnect`: {}.\n",
        note_of(steps, "reconnect", "this socket")
            .map_or_else(|| "not run".to_owned(), |n| cell(&n))
    ));
    out
}

/// One row of THE [U] TABLE.
struct URow {
    question: &'static str,
    steps: &'static [&'static str],
    evidence: String,
}

/// THE [U] TABLE of `briefs/tr.md` §5: each open question of the run, the step that judges it, the
/// verdict from the records and the words of its evidence. A step that did not run leaves its question
/// open.
fn u_rows(steps: &[Value], log: &AppLog, halted: Option<&str>) -> Vec<URow> {
    let notes = |step: &str, prefixes: &[&str]| -> String {
        let found: Vec<String> = prefixes
            .iter()
            .filter_map(|p| note_of(steps, step, p))
            .map(|n| cell(&n))
            .collect();
        if found.is_empty() {
            "(no note)".to_owned()
        } else {
            found.join("; ")
        }
    };
    vec![
        URow {
            question: "The list of test IDs was taken in memory at the code (B1) and removed at the app's first answer; no E6",
            steps: &["link"],
            evidence: format!(
                "{}; E6 lines: {}",
                notes("link", &["allow list:"]),
                log.rows_of("E6_LIST_NOT_USED")
            ),
        },
        URow {
            question: "The phone takes the join (V1_0, D2D V0_3, padded URL-safe base64) and shows the same three emoji",
            steps: &["link"],
            evidence: notes("link", &["emoji:", "the login reached done"]),
        },
        URow {
            question: "P4's client-info accepted; the new-device sync held while Registered waits; a big BlobData cut",
            steps: &["link"],
            evidence: "the link ended `done` and the account went active; the tool does not read the join's inner frames".to_owned(),
        },
        URow {
            question: "The label 'Kourier (<host>)' on the TEST phone's list; the multi-device menu path of the runbook (KH1e)",
            steps: &["link"],
            evidence: notes(
                "link",
                &[
                    "the TEST phone's linked devices list shows",
                    "the menu path on the TEST phone",
                ],
            ),
        },
        URow {
            question: "Sync: time to online, the chats, follower words only in the first count; the held CSP hello after the backlog; live hosts match the pins (no E3)",
            steps: &["sync"],
            evidence: format!(
                "{}; E3 lines: {}",
                notes("sync", &["online ", "first count line:", "no leader promotion", "frame counts: not measured"]),
                log.rows_of("E3_UNTRUSTED")
            ),
        },
        URow {
            question: "One incoming text once in the 1:1 chat and one in the group, the phone leading",
            steps: &["receive", "group-receive"],
            evidence: format!(
                "{}; {}",
                notes("receive", &["one new message"]),
                notes("group-receive", &["one new message"])
            ),
        },
        URow {
            question: "The own message written on the TEST phone shows once, `sent`, then `read` within 5 minutes",
            steps: &["mirror"],
            evidence: notes("mirror", &["the own message's receipt", "the chat was not open", "the open chat's own rows"]),
        },
        URow {
            question: "A quote, a mention, bold text, an edit, a reaction and a delete from the real phone, received by a follower (counts only)",
            steps: &["kinds"],
            evidence: notes("kinds", &["new messages:", "in the chat HE had open", "no chat was open"]),
        },
        URow {
            question: "The leader rule while the TEST phone sleeps (promoted or not, how soon) under its push mode",
            steps: &["phone-sleep"],
            evidence: notes("phone-sleep", &["leader observed in sleep:", "push mode"]),
        },
        URow {
            question: "The leader receive: the promotion's delay, `Syncing` again after it, the counts inside S5b, each text once on both sides, no error on the phone",
            steps: &["flight-mode"],
            evidence: notes(
                "flight-mode",
                &["Kourier took the lead", "Kourier already leads", "account states", "the leader's count line", "the leader's frame counts", "the PERSONAL phone shows the 1:1 text"],
            ),
        },
        URow {
            question: "What the personal phone shows of Kourier's answers as a leader: a forward-security line, the delivery state (the FS `Reject` box is among the leader's job boxes)",
            steps: &["flight-mode"],
            evidence: notes("flight-mode", &["the PERSONAL phone's chat with the test ID", "the PERSONAL phone shows the 1:1 text"]),
        },
        URow {
            question: "After a stop and a start: online with no login step and no 4115; who leads; the earlier texts present once",
            steps: &["reconnect"],
            evidence: notes("reconnect", &["online again", "this socket", "chats of the earlier steps", "no earlier step"]),
        },
        URow {
            question: "Passive with the TEST phone in front: no close, no halt, follower words only",
            steps: &["passive"],
            evidence: notes("passive", &["online at each", "last count line", "the count lines hold"]),
        },
        URow {
            question: "END A: the 4113 path (the phone removes the device; the account reads logged out)",
            steps: &["removed-on-phone"],
            evidence: notes("removed-on-phone", &["close 4113 seen"]),
        },
        URow {
            question: "END B: RP-b's drop (a device may drop itself; the close after the ack) and the TEST phone's list clean",
            steps: &["unlink"],
            evidence: notes("unlink", &["the device's drop", "the TEST phone's linked devices list is clean"]),
        },
        URow {
            question: "No stop rule fired in the whole run",
            steps: &[],
            evidence: halted.map_or_else(
                || "no halt".to_owned(),
                |kind| format!("halted: {}", cell(kind)),
            ),
        },
    ]
}

fn u_section(steps: &[Value], log: &AppLog, halted: Option<&str>) -> String {
    let mut out = String::from(
        "\n## The [U] table (open questions of `briefs/tr.md` §5, filled from the records)\n\n| # | Open question | Step | Verdict | Evidence |\n|---|---|---|---|---|\n",
    );
    for (i, row) in u_rows(steps, log, halted).iter().enumerate() {
        let result = if row.steps.is_empty() {
            if halted.is_some() {
                "finding"
            } else {
                "settled"
            }
        } else {
            verdict(steps, row.steps)
        };
        out.push_str(&format!(
            "| {} | {} | {} | {result} | {} |\n",
            i + 1,
            cell(row.question),
            if row.steps.is_empty() {
                "-".to_owned()
            } else {
                row.steps.join(", ")
            },
            cell(&row.evidence)
        ));
    }
    out.push_str(
        "\n`settled`: the step ended PASS and the evidence is its notes; `finding`: a step ended FAIL, or a rule stopped the run (the evidence says which); `not run`: the question stays open. A leader that was not observed in the sleep is a finding of the leader rule, whatever the step's verdict.\n",
    );
    out
}

/// What stays [U] after the run whatever the records say (`briefs/tr.md` §5.2).
const STAYS_OPEN: &str = "
## What stays [U] after the run

- TH6a's send list beyond receipts: a follower's send, a send at Syncing, the byte-identical repeat unless a retry happens, limits, nickname, the 0x10 flag (RT2).
- 4114's timing; the right-to-left emoji row on the phone; a slot that never got Registered (only after a link that failed after the hello).
- The creator hand-over of the test ID's group, unless a sync request came during the run; KH1f.
";

/// D2: where each answer of a run with a test phone came from (`facts.origins` of the records, the
/// latest per step and question). Words only.
fn origins_section(state: &State, steps: &[Value]) -> String {
    let Some(serial) = state.test_phone() else {
        return String::new();
    };
    let mut out = format!(
        "\n## Answers and where they came from\n\nTest phone: `{}`. Origins: checked on the TEST phone, checked in Kourier's own state, typed by the user, relayed by the lead, not looked at.\n\n| Step | Question | Origin |\n|---|---|---|\n",
        cell(serial)
    );
    let mut rows = 0;
    for s in steps {
        for o in s["facts"]["origins"].as_array().into_iter().flatten() {
            out.push_str(&format!(
                "| {} | {} | {} |\n",
                cell(s["name"].as_str().unwrap_or("?")),
                cell(o["question"].as_str().unwrap_or("?")),
                cell(o["origin"].as_str().unwrap_or("?"))
            ));
            rows += 1;
        }
    }
    if rows == 0 {
        out.push_str("| (none yet) | | |\n");
    }
    out
}

/// The whole report.
pub(super) fn report_text(
    dir: &ProfileDir,
    state: &State,
    steps: &[Value],
    log: &AppLog,
    now: u64,
) -> String {
    let mut out = String::from("# Kourier Threema run\n\n");
    out.push_str(&format!("- Profile: `{}`\n", cell(&dir.label())));
    let build: String = state
        .get("build_commit")
        .as_str()
        .unwrap_or("?")
        .chars()
        .take(10)
        .collect();
    out.push_str(&format!(
        "- Build: {} (core.db schema {})\n",
        cell(&build),
        state
            .num("core_schema")
            .map_or("?".to_owned(), |s| s.to_string())
    ));
    out.push_str(&format!(
        "- Login: {}\n",
        match state.num("linked_since_ms") {
            Some(at) => format!(
                "linked since {}; last online {}",
                utc(at),
                state.num("last_online_ms").map_or("never".to_owned(), utc)
            ),
            None => "not linked".to_owned(),
        }
    ));
    out.push_str(&format!("- Starts: {}\n", state.num("starts").unwrap_or(0)));
    out.push_str(&format!(
        "- Link calls: {}\n",
        steps.iter().filter(|s| s["name"] == "link").count()
    ));
    let halted = state.halt_kind().map(str::to_owned);
    match &halted {
        Some(kind) => {
            let halt = state.get("halted");
            out.push_str(&format!(
                "- Halted: {} at {}{}\n",
                cell(kind),
                halt["at_ms"].as_u64().map_or_else(|| "?".to_owned(), utc),
                halt["drop"].as_str().map_or_else(String::new, |d| format!(
                    " (the device's drop: {})",
                    cell(d)
                ))
            ));
        }
        None => out.push_str("- Halted: no\n"),
    }
    let passed = steps.iter().filter(|s| s["result"] == "PASS").count();
    out.push_str(&format!(
        "- Steps: {} calls, {passed} passed, {} failed\n- Written: {}\n",
        steps.len(),
        steps.len() - passed,
        utc(now)
    ));
    out.push_str(&steps_sections(steps));
    out.push_str(&origins_section(state, steps));
    out.push_str(&sockets_section(log));
    out.push_str(&lines_section(log));
    out.push_str(&leader_section(steps));
    out.push_str(&u_section(steps, log, halted.as_deref()));
    out.push_str(STAYS_OPEN);
    // The last belt: no ID shape and no digest in the file, whatever a note held.
    redact(&out)
}

/// `report`: writes `threema-run/report.md` and says how many steps it holds.
pub(super) fn report_in(dir: &ProfileDir, now: u64) -> Result<Vec<String>> {
    let state = State::load(dir)?.context("not a threema-run profile (no state file)")?;
    let steps = recorded_steps(dir);
    let log = AppLog::read(&dir.app_log());
    write_private(
        &dir.report_file(),
        &report_text(dir, &state, &steps, &log, now),
    )?;
    let passed = steps.iter().filter(|s| s["result"] == "PASS").count();
    Ok(vec![format!(
        "report:   {} ({} step calls, {passed} passed, {} failed)",
        dir.report_file().display(),
        steps.len(),
        steps.len() - passed
    )])
}

fn real_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// `report` as the command line runs it.
pub(super) fn report_command(a: &Args) -> Result<i32> {
    let dir = a.place.resolve(&var, &target_dir())?;
    for line in report_in(&dir, real_now())? {
        println!("{line}");
    }
    Ok(0)
}
