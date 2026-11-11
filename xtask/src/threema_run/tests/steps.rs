//! The steps, part one (T6): `link` (his "TEST phone in hand" before any scan, the emoji, the
//! phone's questions, the stop-rule exits S1a, S1b, S9, S12 and the two "no" ways), `sync`,
//! `receive`, `group-receive` (S7 `lost` and `twice`, the resume), `mirror`, `reconnect` and
//! `passive`, and the step runner's record. The app is a shell script that plays the dev channel on
//! cue under the REAL supervisor; the test is the user (answers through the supervisor's test door,
//! the phones' events and lines through cue files). Nothing starts the real app, nothing contacts a
//! host, no window, no sound. Every wait is on an outcome (a line a step said, an answer, a thread's
//! end); the bounds are failures only. No mutant.

use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::supervise::{Rig, Running, quick};
use super::{args, digest_shape, id_shape};
use crate::threema_run::profile::State;
use crate::threema_run::prompt::QUESTIONS;
use crate::threema_run::steps::{
    Client, STEPS, StepEnd, StepTiming, list_steps, run_step_with, step_command,
};
use crate::threema_run::supervise::{Timing, call_at};

// ---- The stand-in app ---------------------------------------------------------------------------------

/// The app, as a shell script: its dev channel answers `state` and `chats` from files the test
/// writes, records every command it got, and prints on cue the lines the test appends to
/// `events.cue` (stdout) and `lines.cue` (stderr). The line `#exit` in a cue file kills the app at
/// once (a crash). The cue readers end with the app (no process outlives it).
const STAND_IN: &str = r##"#!/bin/sh
APP=$$
D="$STEPS_DIR"
say() { printf '%s\n' "$1"; }
cue() {
  n=0
  while kill -0 "$APP" 2>/dev/null; do
    t=$(wc -l < "$1")
    if [ "$t" -gt "$n" ]; then
      sed -n "$((n+1)),${t}p" "$1" | while IFS= read -r l; do
        case "$l" in
          '#exit') kill -9 "$APP" ;;
          *) printf '%s\n' "$l" ;;
        esac
      done
      n=$t
    fi
    sleep 0.05
  done
}
say '{"type":"event","event":"channel","data":{"protocol":1}}'
cue "$D/events.cue" &
cue "$D/lines.cue" >&2 &
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed 's/.*"id":\([0-9]*\).*/\1/')
  cmd=$(printf '%s' "$line" | sed 's/.*"cmd":"\([a-z_]*\)".*/\1/')
  printf '%s\n' "$cmd" >> "$D/seen"
  case "$cmd" in
    quit) printf '{"type":"response","id":%s,"ok":true,"result":{}}\n' "$id"
      if [ ! -e "$D/hold" ]; then exit 0; fi ;;
    state) printf '{"type":"response","id":%s,"ok":true,"result":%s}\n' "$id" "$(cat "$D/state.json")" ;;
    chats) printf '{"type":"response","id":%s,"ok":true,"result":%s}\n' "$id" "$(cat "$D/chats.json")" ;;
    send|attach) printf '%s\n' "$line" >> "$D/asked"
      printf '{"type":"response","id":%s,"ok":true,"result":{}}\n' "$id" ;;
    *) printf '{"type":"response","id":%s,"ok":true,"result":{}}\n' "$id" ;;
  esac
done
# A held app does not go when asked (its dev channel closes): the test lets it go with `release`,
# else the supervisor kills it after its grace. The run then stays up for a step to read the halt.
while [ -e "$D/hold" ]; do sleep 0.05; done
"##;

const COUNT_ZERO: &str = "2026-11-04T10:00:00.000000Z  INFO kourier_threema::conn::lines: socket frame counts when=running in_reflected=3 in_reflect_ack=0 in_dry=1 in_promoted=0 in_drop_ack=0 in_tx=0 in_csp_message=0 in_message_ack=0 in_queue_complete=1 in_echo=2 in_close_error=0 in_other=0 out_reflected_ack=3 out_reflect_job=0 out_reflect_transmit=0 out_reflect_inbound=0 out_ephemeral=0 out_box_job=0 out_box_transmit=0 out_message_ack=0 out_unblock=0 out_tx=0 out_echo=2 out_idle=1 out_drop=0 out_other=0 held=0 held_by=none";
const D1_LINE: &str = "2026-11-04T10:00:01.000000Z  INFO kourier_threema::conn: incoming messages were unblocked as the leader";
const E1_NORMAL: &str = "2026-11-04T10:00:02.000000Z  INFO kourier_threema::conn: the mediator closed the connection code=1000 meaning=\"Normal closure\"";
const ERROR_LINE: &str = "2026-11-04T10:00:03.000000Z ERROR kourier_core: something broke";
const DROP_REMOVED: &str = "2026-11-04T10:00:05.000000Z  INFO kourier_threema::conn::drop_own: the own device's drop on a socket of its own outcome=Removed";

fn step_event(step: &str, kind: &str) -> Value {
    json!({"type": "event", "event": "login_step", "data": {"account": 1, "step": step, "kind": kind}})
}

fn qr_event() -> Value {
    json!({"type": "event", "event": "login_qr", "data": {"account": 1, "url": "threema://link?x=synthetic"}})
}

fn account_event(state: &str, conn: &str) -> Value {
    json!({"type": "event", "event": "account_state", "data": {"account": 1, "network": "Threema", "state": state, "conn": conn, "device": 2}})
}

fn error_event(message: &str) -> Value {
    json!({"type": "event", "event": "error", "data": {"source": "login", "account": 1, "message": message}})
}

fn chat(id: i64, kind: &str, unread: i64, activity: i64, own: &str) -> Value {
    json!({"conversation": id, "kind": kind, "self": false, "unread": unread, "last_activity": activity, "newest_own": own})
}

/// What the test writes for the app to play.
#[derive(Clone)]
struct Cues(PathBuf);

impl Cues {
    fn append(&self, file: &str, text: &str) {
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(self.0.join(file))
            .expect("the cue file");
        // One write: the app reads whole lines only.
        f.write_all(format!("{text}\n").as_bytes()).expect("a cue");
    }

    fn event(&self, v: &Value) {
        self.append("events.cue", &v.to_string());
    }

    fn line(&self, text: &str) {
        self.append("lines.cue", text);
    }

    fn crash(&self) {
        self.append("events.cue", "#exit");
    }

    fn put(&self, file: &str, v: &Value) {
        let tmp = self.0.join(format!("{file}.tmp"));
        std::fs::write(&tmp, v.to_string()).expect("a file");
        std::fs::rename(tmp, self.0.join(file)).expect("a rename");
    }

    fn chats(&self, chats: &[Value]) {
        self.put("chats.json", &json!({"chats": chats}));
    }
}

/// The lines a step said, in order, and the gate of his answers: while it is shut he answers
/// nothing (a scenario opens it when the phones are where the test wants them).
struct Hear {
    rx: Receiver<String>,
    gate: Arc<AtomicBool>,
}

impl Hear {
    /// Blocks until a line holding `pattern` was said (the lines before it are dropped).
    fn until(&self, pattern: &str) -> String {
        let limit = Instant::now() + Duration::from_secs(30);
        loop {
            let left = limit.saturating_duration_since(Instant::now());
            match self.rx.recv_timeout(left) {
                Ok(line) if line.contains(pattern) => return line,
                Ok(_) => {}
                Err(_) => panic!("the step never said \"{pattern}\""),
            }
        }
    }

    /// He may answer now.
    fn answers_now(&self) {
        self.gate.store(true, Ordering::SeqCst);
    }
}

/// A profile with a stand-in app under the real supervisor.
struct Stage {
    rig: Rig,
    cues: Cues,
}

impl Stage {
    fn new(name: &str) -> Self {
        Self::in_place(name, false)
    }

    /// A stage in a rehearsal's place: the fake that plays Threema prints no count line there, so a
    /// step may say "not measured" and pass (MEDIUM-1).
    fn rehearsing(name: &str) -> Self {
        Self::in_place(name, true)
    }

    fn in_place(name: &str, rehearsal: bool) -> Self {
        let rig = Rig::new(name);
        let rig = if rehearsal { rig.rehearsing() } else { rig };
        let dir = rig.w.scratch.0.join("cue");
        std::fs::create_dir_all(&dir).expect("the cue directory");
        for file in ["events.cue", "lines.cue", "seen", "hold"] {
            std::fs::write(dir.join(file), "").expect("a cue file");
        }
        let cues = Cues(dir);
        cues.put("state.json", &json!({"open": 0, "rows": []}));
        cues.chats(&[]);
        Self { rig, cues }
    }

    /// The profile holds an account (a login stood and ended `done` in an earlier run).
    fn linked(&self) {
        let mut state = State::load(&self.rig.dir)
            .expect("the state")
            .expect("a state file");
        state.set("linked_since_ms", json!(5));
        state.save(&self.rig.dir).expect("saved");
    }

    /// The app may go when it is asked (a run that ends is waited for after this).
    fn release(&self) {
        let _ = std::fs::remove_file(self.cues.0.join("hold"));
    }

    /// Starts the supervisor: a `--for-link` start has its ID typed at once (the test door), then
    /// the app is up. The app is HELD (it does not go when asked, a halt's end kills it after five
    /// seconds): a step that reads a halt finds the supervisor still there.
    fn start(&self, for_link: bool) -> Running {
        let timing = Timing {
            kill_grace: Duration::from_secs(5),
            ..quick()
        };
        let mut run = self.rig.run_script(
            STAND_IN,
            vec![("STEPS_DIR", self.cues.0.display().to_string())],
            for_link,
            timing,
        );
        if for_link {
            run.wait_for("the wait for the ID", |i| i["id"] == json!("waiting"));
            run.call(&json!({"op": "test_id_arrived"}))
                .expect("the gate");
        }
        run.ready();
        run
    }

    /// The app got these commands, one a line.
    fn seen(&self) -> Vec<String> {
        std::fs::read_to_string(self.cues.0.join("seen"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn records(&self) -> Vec<Value> {
        std::fs::read_to_string(self.rig.dir.steps_log())
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).expect("a record"))
            .collect()
    }
}

thread_local! {
    /// The TEST phone the next step call of this thread gets (a scripted one; no test opens adb).
    static PHONE: std::cell::RefCell<Option<anyhow::Result<Box<dyn crate::test_phone::Phone>>>> =
        const { std::cell::RefCell::new(None) };
}

/// The next step call of this thread runs with this TEST phone.
fn give_phone(phone: crate::test_phone::FakePhone) {
    PHONE.with(|c| *c.borrow_mut() = Some(Ok(Box::new(phone))));
}

/// The next step call of this thread finds the TEST phone unreachable.
fn give_no_phone() {
    PHONE.with(|c| *c.borrow_mut() = Some(Err(anyhow::anyhow!("unreachable"))));
}

/// Short bounds: every one is an outcome, none is measured.
fn short() -> StepTiming {
    StepTiming {
        poll: Duration::from_millis(10),
        call_max: Duration::from_secs(20),
        ready_wait: Duration::from_secs(10),
        qr_wait: Duration::from_secs(10),
        online_wait: Duration::from_secs(10),
        answer_wait: Duration::from_secs(10),
        settle: Duration::from_millis(100),
        baseline_wait: Duration::from_secs(10),
        lost_after: Duration::from_secs(10),
        read_wait: Duration::from_secs(10),
        count_wait: Duration::from_secs(10),
        drop_look: Duration::from_secs(10),
        look: Duration::from_millis(20),
        sleep_text_at: Duration::from_millis(300),
        sleep_total: Duration::from_millis(700),
        d1_wait: Duration::from_secs(10),
        later_text_after: Duration::from_millis(200),
        removal_wait: Duration::from_secs(10),
    }
}

/// One call of a step, with the lines it said handed to `heard`.
fn call_step(
    stage: &Stage,
    name: &str,
    extra: &[&str],
    timing: StepTiming,
    heard: &dyn Fn(&str),
) -> StepEnd {
    call_step_at(stage, stage.rig.socket(), name, extra, timing, heard)
}

/// [`call_step`], the step's client talking to the supervisor through `socket` (a [`Door`]).
fn call_step_at(
    stage: &Stage,
    socket: PathBuf,
    name: &str,
    extra: &[&str],
    timing: StepTiming,
    heard: &dyn Fn(&str),
) -> StepEnd {
    let def = STEPS.iter().find(|d| d.name == name).expect("a step");
    let mut words = vec!["step", name];
    words.extend_from_slice(extra);
    let a = args(&words).expect("the command line");
    let phone = PHONE.with(|c| c.borrow_mut().take());
    run_step_with(
        &a,
        &stage.rig.dir,
        &Client::at(socket),
        def,
        timing,
        heard,
        phone,
    )
    .expect("the record is written")
}

/// Runs a step while `scenario` plays the phones (it hears what the step says) and an answerer
/// plays him: each time the step ASKS a question in `answers` (a `question_asked` event of the
/// supervisor) and it is open, he answers it once.
fn drive<S>(
    stage: &Stage,
    name: &str,
    extra: &[&str],
    timing: StepTiming,
    answers: &[(&'static str, bool)],
    scenario: S,
) -> StepEnd
where
    S: FnOnce(&Cues, &Hear) + Send + 'static,
{
    drive_gated(stage, name, extra, timing, answers, false, scenario)
}

/// [`drive`], with his answers held back until the scenario says `answers_now` when `held`.
fn drive_gated<S>(
    stage: &Stage,
    name: &str,
    extra: &[&str],
    timing: StepTiming,
    answers: &[(&'static str, bool)],
    held: bool,
    scenario: S,
) -> StepEnd
where
    S: FnOnce(&Cues, &Hear) + Send + 'static,
{
    drive_at(
        stage,
        stage.rig.socket(),
        (name, extra, timing),
        answers,
        held,
        scenario,
    )
}

/// [`drive_gated`], the STEP talking to the supervisor through `step_socket` (a [`Door`]; the answerer talks to it
/// directly).
fn drive_at<S>(
    stage: &Stage,
    step_socket: PathBuf,
    (name, extra, timing): (&str, &[&str], StepTiming),
    answers: &[(&'static str, bool)],
    held: bool,
    scenario: S,
) -> StepEnd
where
    S: FnOnce(&Cues, &Hear) + Send + 'static,
{
    for (question, _) in answers {
        assert!(
            QUESTIONS.iter().any(|(q, _)| q == question),
            "an unknown question"
        );
    }
    let (tx, rx) = channel::<String>();
    let tx = Mutex::new(tx);
    let cues = stage.cues.clone();
    let gate = Arc::new(AtomicBool::new(!held));
    let hear = Hear {
        rx,
        gate: Arc::clone(&gate),
    };
    let socket = stage.rig.socket();
    // Only what is asked from now on is answered (a question left open by an earlier call is not).
    let mut since = call_at(
        &socket,
        &json!({"op": "events", "since": 0}),
        Duration::from_secs(5),
    )
    .map_or(0, |e| e["next_seq"].as_u64().unwrap_or(0));
    let scenario = std::thread::spawn(move || scenario(&cues, &hear));
    let done = Arc::new(AtomicBool::new(false));
    let answered: Vec<(&'static str, bool)> = answers.to_vec();
    let flag = Arc::clone(&done);
    let answerer = std::thread::spawn(move || {
        let mut asked: Vec<String> = Vec::new();
        while !flag.load(Ordering::SeqCst) {
            if let Ok(events) = call_at(
                &socket,
                &json!({"op": "events", "since": since}),
                Duration::from_secs(5),
            ) {
                since = events["next_seq"].as_u64().unwrap_or(since);
                for e in events["events"].as_array().into_iter().flatten() {
                    if e["event"] == "question_asked"
                        && let Some(q) = e["data"]["name"].as_str()
                    {
                        asked.push(q.to_owned());
                    }
                }
            }
            if gate.load(Ordering::SeqCst)
                && let Ok(info) = call_at(&socket, &json!({"op": "info"}), Duration::from_secs(5))
                && let Some(open) = info["question"].as_str()
                && let Some(at) = asked.iter().position(|q| q == open)
                && let Some((question, yes)) = answered.iter().find(|(q, _)| *q == open)
            {
                let _ = call_at(
                    &socket,
                    &json!({"op": "test_answer", "name": question, "yes": yes}),
                    Duration::from_secs(5),
                );
                asked.remove(at);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    });
    let end = call_step_at(stage, step_socket, name, extra, timing, &|line| {
        let _ = tx.lock().expect("the channel").send(line.to_owned());
    });
    done.store(true, Ordering::SeqCst);
    answerer.join().expect("the answerer");
    scenario.join().expect("the scenario");
    end
}

/// The run ends: a run that halted ends by itself, any other is stopped.
fn finish(stage: &Stage, run: Running) {
    stage.release();
    let _ = run.call(&json!({"op": "stop"}));
    let ended = run.join_within(Duration::from_secs(30));
    assert!(ended.is_some_and(|r| r.is_ok()), "the supervisor ended");
}

fn said(end: &StepEnd, pattern: &str) -> bool {
    end.said.iter().any(|l| l.contains(pattern))
}

fn nothing_of_a_value(end: &StepEnd, stage: &Stage) {
    let all = format!(
        "{} {} {} {}",
        end.reason,
        end.notes.join(" "),
        end.said.join(" "),
        std::fs::read_to_string(stage.rig.dir.steps_log()).unwrap_or_default()
    );
    assert!(
        !all.contains(&id_shape()) && !all.contains(&digest_shape()),
        "{all}"
    );
}

const ALL_PHONE_ANSWERS: [(&str, bool); 5] = [
    ("test-phone-in-hand", true),
    ("emoji-equal", true),
    ("list-shows-kourier", true),
    ("menu-path-as-runbook", true),
    ("phone-warning", false),
];

// ---- link ------------------------------------------------------------------------------------------------------

#[test]
fn link_asks_first_scans_only_after_his_yes_and_ends_with_the_phone_questions() {
    let stage = Stage::new("t6-link");
    let run = stage.start(true);
    let end = drive(
        &stage,
        "link",
        &[],
        short(),
        &ALL_PHONE_ANSWERS,
        |cues, hear| {
            hear.until("WINDOW:");
            cues.event(&step_event("qr", "qr"));
            cues.event(&qr_event());
            hear.until("scan now");
            cues.event(&step_event("emoji", "emoji"));
            hear.until("compare the three emoji");
            cues.event(&step_event("linking", "progress"));
            cues.event(&step_event("done", "done"));
            cues.event(&account_event("active", "syncing"));
        },
    );
    assert_eq!(end.status, 0, "{}", end.reason);
    // The question about the phone was asked before the code was told to anybody.
    let at = |p: &str| end.said.iter().position(|l| l.contains(p)).expect(p);
    assert!(
        at("Is the phone in your hand") < at("scan now"),
        "{:?}",
        end.said
    );
    assert!(end.said[at("Is the phone in your hand")].starts_with("TERMINAL:"));
    assert!(said(&end, "the personal phone stays face down"));
    assert!(
        end.notes
            .iter()
            .any(|n| n.contains("the account is active"))
    );
    assert!(end.notes.iter().any(|n| n.contains("allow list: none")));
    assert!(
        end.notes
            .iter()
            .any(|n| n.contains("emoji: he said the three were equal"))
    );
    // Written down: one PASS record, no value.
    let records = stage.records();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["name"], json!("link"));
    assert_eq!(records[0]["result"], json!("PASS"));
    nothing_of_a_value(&end, &stage);
    // The run is not halted and still up; the step closed nothing and sent nothing.
    let info = run.info().expect("info");
    assert!(info["halted"].is_null(), "{info}");
    assert_eq!(info["question"], Value::Null);
    assert!(
        !stage.seen().iter().any(|c| c == "page"),
        "{:?}",
        stage.seen()
    );
    finish(&stage, run);
}

#[test]
fn link_never_says_scan_now_while_he_has_not_answered_the_question() {
    let stage = Stage::new("t6-link-silent");
    let run = stage.start(true);
    let timing = StepTiming {
        call_max: Duration::from_millis(1500),
        ..short()
    };
    let end = drive(&stage, "link", &[], timing, &[], |cues, hear| {
        hear.until("WINDOW:");
        cues.event(&step_event("qr", "qr"));
        cues.event(&qr_event());
    });
    assert_eq!(end.status, 1);
    assert!(!said(&end, "scan now"), "{:?}", end.said);
    assert!(
        said(&end, "Do NOT scan it until you answered"),
        "{:?}",
        end.said
    );
    assert!(end.reason.contains("time ran out"), "{}", end.reason);
    // Nothing was closed and nothing halted: the login may still be picked up.
    assert!(run.info().expect("info")["halted"].is_null());
    assert!(!stage.seen().iter().any(|c| c == "page"));
    // A later call picks the login up where it stands (the question is asked again, fresh).
    let again = drive(
        &stage,
        "link",
        &[],
        short(),
        &ALL_PHONE_ANSWERS,
        |cues, hear| {
            hear.until("scan now");
            cues.event(&step_event("emoji", "emoji"));
            hear.until("compare the three emoji");
            cues.event(&step_event("linking", "progress"));
            cues.event(&step_event("done", "done"));
            cues.event(&account_event("active", "syncing"));
        },
    );
    assert_eq!(again.status, 0, "{}", again.reason);
    assert!(again.notes.iter().any(|n| n.contains("already underway")));
    finish(&stage, run);
}

#[test]
fn link_picks_a_login_up_at_linking_and_its_own_mark_is_no_question() {
    let stage = Stage::new("t6-link-resume");
    let run = stage.start(true);
    let timing = StepTiming {
        call_max: Duration::from_millis(800),
        ..short()
    };
    let answers = [("test-phone-in-hand", true)];
    let first = drive(&stage, "link", &[], timing, &answers, |cues, hear| {
        hear.until("WINDOW:");
        cues.event(&step_event("qr", "qr"));
        cues.event(&qr_event());
        hear.until("scan now");
        cues.event(&step_event("emoji", "emoji"));
        cues.event(&step_event("linking", "progress"));
    });
    assert_eq!(first.status, 1);
    assert!(first.reason.contains("time ran out"), "{}", first.reason);
    // The login stands at `linking`: the keeper root holds the run's own mark, which is no question.
    assert!(stage.rig.dir.open_question_file().exists());
    let all = ALL_PHONE_ANSWERS;
    let second = drive(&stage, "link", &[], short(), &all, |cues, hear| {
        // The step has taken the login up (it says it waits) before the phone's last word comes.
        hear.until("WAIT:");
        cues.event(&step_event("done", "done"));
        cues.event(&account_event("active", "syncing"));
    });
    assert_eq!(second.status, 0, "{}", second.reason);
    assert!(second.notes.iter().any(|n| n.contains("already underway")));
    // It asked no "TEST phone in hand" again: the phone had scanned already.
    assert!(
        !second
            .said
            .iter()
            .any(|l| l.contains("Is the phone in your hand"))
    );
    // `done` ends the mark.
    assert!(!stage.rig.dir.open_question_file().exists());
    finish(&stage, run);
}

#[test]
fn link_his_no_closes_the_page_halts_nothing_and_scans_nothing() {
    let stage = Stage::new("t6-link-no");
    let run = stage.start(true);
    let socket = stage.rig.socket();
    let end = drive_gated(
        &stage,
        "link",
        &[],
        short(),
        &[("test-phone-in-hand", false)],
        true,
        move |cues, hear| {
            hear.until("WINDOW:");
            cues.event(&step_event("qr", "qr"));
            cues.event(&qr_event());
            // The supervisor holds the code's page before he answers no.
            let limit = Instant::now() + Duration::from_secs(30);
            while call_at(&socket, &json!({"op": "info"}), Duration::from_secs(5))
                .map_or(true, |i| i["login"]["step"] != json!("qr"))
            {
                assert!(Instant::now() < limit, "the login never showed its code");
                std::thread::sleep(Duration::from_millis(5));
            }
            hear.answers_now();
        },
    );
    assert_eq!(end.status, 1);
    assert!(end.reason.contains("NOT the TEST phone"), "{}", end.reason);
    assert!(!said(&end, "scan now"), "{:?}", end.said);
    // The page change to the list went to the app; the run is not halted.
    assert!(
        stage.seen().iter().any(|c| c == "page"),
        "{:?}",
        stage.seen()
    );
    let info = run.info().expect("info");
    assert!(info["halted"].is_null(), "{info}");
    assert!(
        end.reason.contains("link page was closed"),
        "{}",
        end.reason
    );
    assert!(end.notes.iter().any(|n| n.contains("link page was closed")));
    finish(&stage, run);

    // His no before any code: nothing to close, and the words do not claim a close.
    let stage = Stage::new("t6-link-no-early");
    let run = stage.start(true);
    let end = drive(
        &stage,
        "link",
        &[],
        short(),
        &[("test-phone-in-hand", false)],
        |_, _| {},
    );
    assert_eq!(end.status, 1);
    assert!(end.reason.contains("NOT the TEST phone"), "{}", end.reason);
    assert!(end.reason.contains("no code was up"), "{}", end.reason);
    assert!(!end.reason.contains("page was closed"), "{}", end.reason);
    assert!(!stage.seen().iter().any(|c| c == "page"));
    finish(&stage, run);
}

#[test]
fn link_he_says_the_emoji_differ_so_the_run_stops() {
    let stage = Stage::new("t6-link-emoji");
    let run = stage.start(true);
    let answers = [("test-phone-in-hand", true), ("emoji-equal", false)];
    let end = drive(&stage, "link", &[], short(), &answers, |cues, hear| {
        hear.until("WINDOW:");
        cues.event(&step_event("qr", "qr"));
        cues.event(&qr_event());
        hear.until("scan now");
        cues.event(&step_event("emoji", "emoji"));
    });
    assert_eq!(end.status, 1);
    assert!(
        end.reason.contains("CANCEL on the TEST phone"),
        "{}",
        end.reason
    );
    let info = run.info();
    // The judge halted `stopped` (S12): the supervisor's end closes the waiting login itself.
    let halted = info.map_or(Value::Null, |i| i["halted"].clone());
    assert!(halted == json!("stopped") || halted.is_null(), "{halted}");
    finish_halted(run, "stopped", &stage);
}

/// The run of a halt ends by itself; its state says the kind.
fn finish_halted(run: Running, kind: &str, stage: &Stage) {
    stage.release();
    let ended = run.join_within(Duration::from_secs(30));
    assert!(ended.is_some_and(|r| r.is_ok()), "the halted run ended");
    let state = stage.rig.state();
    assert_eq!(state["halted"]["kind"], json!(kind), "{state}");
}

#[test]
fn link_s1a_a_refused_pairing_says_nothing_is_registered() {
    let stage = Stage::new("t6-link-1a");
    let run = stage.start(true);
    let answers = [("test-phone-in-hand", true)];
    let end = drive(&stage, "link", &[], short(), &answers, |cues, hear| {
        hear.until("WINDOW:");
        cues.event(&step_event("qr", "qr"));
        cues.event(&qr_event());
        hear.until("scan now");
        cues.event(&step_event("emoji", "emoji"));
        cues.event(&step_event("linking", "progress"));
        cues.event(&step_event("linking", "failed"));
        cues.event(&error_event(
            "NotTheExpectedAccount: the account is not the expected one",
        ));
    });
    assert_eq!(end.status, 1);
    assert!(end.reason.contains("link-refused"), "{}", end.reason);
    assert!(
        end.reason.contains("nothing is registered"),
        "{}",
        end.reason
    );
    assert!(!end.reason.contains("MAY be registered"));
    finish_halted(run, "link-refused", &stage);
    // No question: nothing may be registered.
    assert!(!stage.rig.dir.open_question_file().exists());
}

#[test]
fn link_s1a_the_list_not_taken_at_the_code_is_a_refusal_before_any_socket() {
    let stage = Stage::new("t6-link-list");
    let run = stage.start(true);
    let answers = [("test-phone-in-hand", true)];
    let end = drive(&stage, "link", &[], short(), &answers, |cues, hear| {
        hear.until("WINDOW:");
        cues.event(&step_event("qr", "failed"));
        cues.event(&error_event("NotTheExpectedAccount: the list is empty"));
    });
    assert_eq!(end.status, 1);
    assert!(end.reason.contains("list-not-taken"), "{}", end.reason);
    assert!(
        end.reason.contains("nothing is registered"),
        "{}",
        end.reason
    );
    finish_halted(run, "list-not-taken", &stage);
}

#[test]
fn link_s1b_a_failure_after_the_hello_sends_him_to_the_phones_list_and_then_to_answer() {
    let stage = Stage::new("t6-link-1b");
    let run = stage.start(true);
    let answers = [("test-phone-in-hand", true)];
    let end = drive(&stage, "link", &[], short(), &answers, |cues, hear| {
        hear.until("WINDOW:");
        cues.event(&step_event("qr", "qr"));
        cues.event(&qr_event());
        hear.until("scan now");
        cues.event(&step_event("emoji", "emoji"));
        cues.event(&step_event("linking", "progress"));
        cues.event(&step_event("linking", "failed"));
        cues.event(&error_event("Internal: the core refused the first row"));
        // RP-b's drop on a socket of its own came to Removed.
        cues.line(DROP_REMOVED);
    });
    assert_eq!(end.status, 1);
    assert!(
        end.reason.contains("link-failed-after-hello"),
        "{}",
        end.reason
    );
    assert!(end.reason.contains("MAY be registered"), "{}", end.reason);
    assert!(end.reason.contains("linked devices"), "{}", end.reason);
    assert!(end.reason.contains("threema-run answer"), "{}", end.reason);
    assert!(
        end.reason.contains("the mediator said Removed"),
        "{}",
        end.reason
    );
    finish_halted(run, "link-failed-after-hello", &stage);
    // The question stands in the keeper root until he answers it: no new link before that.
    assert!(stage.rig.dir.open_question_file().exists());
    nothing_of_a_value(&end, &stage);
}

#[test]
fn link_s1b_an_app_that_dies_while_the_login_stands_at_linking_says_the_same() {
    let stage = Stage::new("t6-link-crash");
    let run = stage.start(true);
    let answers = [("test-phone-in-hand", true)];
    let end = drive(&stage, "link", &[], short(), &answers, |cues, hear| {
        hear.until("WINDOW:");
        cues.event(&step_event("qr", "qr"));
        cues.event(&qr_event());
        hear.until("scan now");
        cues.event(&step_event("linking", "progress"));
        cues.crash();
    });
    assert_eq!(end.status, 1);
    assert!(end.reason.contains("MAY be registered"), "{}", end.reason);
    assert!(end.reason.contains("linked devices"), "{}", end.reason);
    stage.release();
    let ended = run.join_within(Duration::from_secs(30));
    assert!(ended.is_some(), "the supervisor ended");
    assert!(stage.rig.dir.open_question_file().exists());
}

/// A door between a step and the supervisor's socket (RT1 attempt 1's order, made certain): every request goes through,
/// until `pass` answers of `info` carried a halt; the request after them WAITS until the test opens the door, and then
/// goes to a supervisor that has ended (its socket is gone), so the step's call fails unanswered, as a supervisor that
/// ended between two looks of the step.
#[derive(Clone)]
struct Door {
    path: PathBuf,
    state: Arc<(Mutex<DoorState>, Condvar)>,
}

#[derive(Default)]
struct DoorState {
    halted_answers: usize,
    held: bool,
    open: bool,
    /// The held request found the supervisor gone.
    found_gone: bool,
}

impl Door {
    fn new(path: PathBuf, to: PathBuf, pass: usize) -> Self {
        let listener = UnixListener::bind(&path).expect("the door's socket");
        let state = Arc::new((Mutex::new(DoorState::default()), Condvar::new()));
        let shared = Arc::clone(&state);
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let shared = Arc::clone(&shared);
                let to = to.clone();
                std::thread::spawn(move || door_serve(stream, &to, &shared, pass));
            }
        });
        Self { path, state }
    }

    /// Blocks until a request waits at the door (a wall bound: a failure only).
    fn wait_held(&self) {
        let (m, cv) = &*self.state;
        let limit = Instant::now() + Duration::from_secs(30);
        let mut s = m.lock().expect("the door");
        while !s.held {
            let left = limit.saturating_duration_since(Instant::now());
            assert!(
                !left.is_zero(),
                "no request came to the door after the halt"
            );
            s = cv.wait_timeout(s, left).expect("the door").0;
        }
    }

    fn open(&self) {
        let (m, cv) = &*self.state;
        m.lock().expect("the door").open = true;
        cv.notify_all();
    }

    fn found_gone(&self) -> bool {
        self.state.0.lock().expect("the door").found_gone
    }
}

/// One request through the door (one JSON line each way, as the supervisor's socket).
fn door_serve(
    stream: UnixStream,
    to: &std::path::Path,
    state: &(Mutex<DoorState>, Condvar),
    pass: usize,
) {
    let mut line = String::new();
    let Ok(reader) = stream.try_clone() else {
        return;
    };
    if BufReader::new(reader).read_line(&mut line).is_err() {
        return;
    }
    let (m, cv) = state;
    let held = {
        let mut s = m.lock().expect("the door");
        if s.halted_answers >= pass {
            s.held = true;
            cv.notify_all();
            while !s.open {
                s = cv.wait(s).expect("the door");
            }
            true
        } else {
            false
        }
    };
    let Ok(mut up) = UnixStream::connect(to) else {
        // The supervisor is gone: the step's connection closes unanswered.
        if held {
            m.lock().expect("the door").found_gone = true;
        }
        return;
    };
    if writeln!(up, "{}", line.trim_end()).is_err() {
        return;
    }
    let mut answer = String::new();
    if BufReader::new(up).read_line(&mut answer).is_err() {
        return;
    }
    let asked_info = serde_json::from_str::<Value>(&line).is_ok_and(|r| r["op"] == "info");
    let halted = serde_json::from_str::<Value>(&answer).is_ok_and(|a| a["halted"].is_string());
    if asked_info && halted {
        m.lock().expect("the door").halted_answers += 1;
    }
    let mut stream = stream;
    let _ = stream.write_all(answer.as_bytes());
}

/// No words after the step's name.
const NO_EXTRA: &[&str] = &[];

#[test]
fn link_s1b_a_drop_line_right_before_the_supervisor_ends_is_read_from_the_profile() {
    let stage = Stage::new("t6-link-1b-gone");
    let run = stage.start(true);
    let door = Door::new(
        stage.rig.socket().with_file_name("door"),
        stage.rig.socket(),
        2,
    );
    let answers = [("test-phone-in-hand", true)];
    let opened = door.clone();
    let end = drive_at(
        &stage,
        door.path.clone(),
        ("link", NO_EXTRA, short()),
        &answers,
        false,
        move |cues, hear| {
            hear.until("WINDOW:");
            cues.event(&step_event("qr", "qr"));
            cues.event(&qr_event());
            hear.until("scan now");
            cues.event(&step_event("emoji", "emoji"));
            cues.event(&step_event("linking", "progress"));
            cues.event(&step_event("linking", "failed"));
            cues.event(&error_event("Internal: the core refused the first row"));
            // The step saw the halt and looked once for the drop's outcome (none yet): its next look waits at the door.
            opened.wait_held();
            // RP-b's drop on a socket of its own came to Removed, right before the supervisor ends (the app may go).
            let _ = std::fs::remove_file(cues.0.join("hold"));
            cues.line(DROP_REMOVED);
            let ended = run.join_within(Duration::from_secs(30));
            assert!(ended.is_some_and(|r| r.is_ok()), "the supervisor ended");
            opened.open();
        },
    );
    assert!(
        door.found_gone(),
        "the step's look after the halt did not find the supervisor gone"
    );
    assert_eq!(end.status, 1);
    assert!(
        end.reason.contains("link-failed-after-hello"),
        "{}",
        end.reason
    );
    assert!(end.reason.contains("MAY be registered"), "{}", end.reason);
    assert!(
        end.reason.contains("the mediator said Removed"),
        "{}",
        end.reason
    );
    assert!(!end.reason.contains("not confirmed"), "{}", end.reason);
    let state = stage.rig.state();
    assert_eq!(
        state["halted"]["kind"],
        json!("link-failed-after-hello"),
        "{state}"
    );
    assert_eq!(state["halted"]["drop"], json!("removed"), "{state}");
    assert!(stage.rig.dir.open_question_file().exists());
    nothing_of_a_value(&end, &stage);
}

#[test]
fn link_s9_a_warning_on_a_phone_halts_the_run() {
    let stage = Stage::new("t6-link-warn");
    let run = stage.start(true);
    let mut answers = ALL_PHONE_ANSWERS;
    answers[4] = ("phone-warning", true);
    let end = drive(&stage, "link", &[], short(), &answers, |cues, hear| {
        hear.until("WINDOW:");
        cues.event(&step_event("qr", "qr"));
        cues.event(&qr_event());
        hear.until("scan now");
        cues.event(&step_event("linking", "progress"));
        cues.event(&step_event("done", "done"));
        cues.event(&account_event("active", "syncing"));
    });
    assert_eq!(end.status, 1);
    assert!(end.reason.contains("phone-warning"), "{}", end.reason);
    finish_halted(run, "phone-warning", &stage);
}

#[test]
fn link_a_list_without_kourier_is_a_finding_not_a_halt() {
    let stage = Stage::new("t6-link-list-no");
    let run = stage.start(true);
    let mut answers = ALL_PHONE_ANSWERS;
    answers[2] = ("list-shows-kourier", false);
    let end = drive(&stage, "link", &[], short(), &answers, |cues, hear| {
        hear.until("WINDOW:");
        cues.event(&step_event("qr", "qr"));
        cues.event(&qr_event());
        hear.until("scan now");
        cues.event(&step_event("linking", "progress"));
        cues.event(&step_event("done", "done"));
        cues.event(&account_event("active", "syncing"));
    });
    assert_eq!(end.status, 1);
    assert!(
        end.reason.contains("does NOT show a Kourier entry"),
        "{}",
        end.reason
    );
    assert!(run.info().expect("info")["halted"].is_null());
    finish(&stage, run);
}

#[test]
fn link_refuses_before_the_id_in_a_start_not_for_a_link_on_a_profile_with_an_account_and_while_a_question_stands()
 {
    // The ID is not typed: the supervisor waits, no app yet.
    let stage = Stage::new("t6-link-id");
    let mut run = stage.rig.run_script(
        STAND_IN,
        vec![("STEPS_DIR", stage.cues.0.display().to_string())],
        true,
        quick(),
    );
    run.wait_for("the wait for the ID", |i| i["id"] == json!("waiting"));
    let end = call_step(&stage, "link", &[], short(), &|_| {});
    assert_eq!(end.status, 1);
    assert!(end.reason.contains("threema-run id"), "{}", end.reason);
    assert!(end.said.is_empty());
    let _ = run.call(&json!({"op": "stop"}));
    assert!(run.join_within(Duration::from_secs(30)).is_some());

    // A start that was not for a link.
    let stage = Stage::new("t6-link-plain");
    let run = stage.start(false);
    let end = call_step(&stage, "link", &[], short(), &|_| {});
    assert_eq!(end.status, 1);
    assert!(end.reason.contains("not for a link"), "{}", end.reason);
    finish(&stage, run);

    // A profile that holds an account.
    let stage = Stage::new("t6-link-account");
    stage.linked();
    let run = stage.start(true);
    let end = call_step(&stage, "link", &[], short(), &|_| {});
    assert_eq!(end.status, 1);
    assert!(
        end.reason.contains("has an account already"),
        "{}",
        end.reason
    );
    assert!(end.said.is_empty());
    finish(&stage, run);

    // The question of a failed link stands in the keeper root.
    let stage = Stage::new("t6-link-question");
    let run = stage.start(true);
    std::fs::write(
        stage.rig.dir.open_question_file(),
        "{\"kind\": \"link-failed-after-hello\", \"at_ms\": 1}\n",
    )
    .expect("a question");
    let end = call_step(&stage, "link", &[], short(), &|_| {});
    assert_eq!(end.status, 1);
    assert!(
        end.reason.contains("question of a failed link"),
        "{}",
        end.reason
    );
    assert!(
        end.said.is_empty(),
        "nothing was said or asked: {:?}",
        end.said
    );
    finish(&stage, run);
}

// ---- sync ---------------------------------------------------------------------------------------------------------

#[test]
fn sync_reads_the_account_the_chats_and_the_first_counts() {
    let stage = Stage::new("t6-sync");
    stage.linked();
    stage.cues.chats(&[
        chat(1, "direct", 0, 1000, "none"),
        chat(2, "group", 0, 900, "none"),
    ]);
    let mut run = stage.start(false);
    stage.cues.event(&account_event("active", "syncing"));
    stage.cues.event(&account_event("active", "online"));
    stage.cues.line(COUNT_ZERO);
    run.wait_for("online", |i| i["online"] == json!(true));
    let end = call_step(&stage, "sync", &[], short(), &|_| {});
    assert_eq!(end.status, 0, "{}", end.reason);
    let notes = end.notes.join("\n");
    assert!(notes.contains("syncing > online"), "{notes}");
    assert!(
        notes.contains("1 direct and 1 group conversations"),
        "{notes}"
    );
    assert!(notes.contains("when=running in_reflected=3"), "{notes}");
    assert!(notes.contains("follower words only"), "{notes}");
    assert!(notes.contains("no leader promotion was seen"), "{notes}");
    let record = &stage.records()[0];
    assert_eq!(record["result"], json!("PASS"));
    assert_eq!(record["facts"]["chats"].as_array().map(Vec::len), Some(2));
    nothing_of_a_value(&end, &stage);
    finish(&stage, run);
}

#[test]
fn sync_without_a_count_line_says_not_measured_in_a_rehearsal_and_a_promotion_is_a_finding() {
    let stage = Stage::rehearsing("t6-sync-bare");
    stage.linked();
    let mut run = stage.start(false);
    stage.cues.event(&account_event("active", "online"));
    run.wait_for("online", |i| i["online"] == json!(true));
    let timing = StepTiming {
        count_wait: Duration::from_millis(300),
        ..short()
    };
    let end = call_step(&stage, "sync", &[], timing, &|_| {});
    assert_eq!(end.status, 0, "{}", end.reason);
    assert!(
        end.notes.iter().any(|n| n.contains("not measured")),
        "{:?}",
        end.notes
    );
    // D1 while the phone should lead: not a halt, a FAIL with its words.
    stage.cues.line(D1_LINE);
    stage.cues.line(COUNT_ZERO);
    run.wait_for("leader", |i| i["leader"] == json!(true));
    let end = call_step(&stage, "sync", &[], short(), &|_| {});
    assert_eq!(end.status, 1);
    assert!(end.reason.contains("became the leader"), "{}", end.reason);
    assert!(run.info().expect("info")["halted"].is_null());
    finish(&stage, run);
}

/// MEDIUM-1: in a REAL place a `sync` that sees no count line FAILS (the count rules S5a-d and S6
/// would judge nothing for the whole run); the rehearsal's fake prints none and may pass (above).
#[test]
fn sync_without_a_count_line_fails_in_a_real_place() {
    let stage = Stage::new("t6-sync-blind");
    stage.linked();
    let mut run = stage.start(false);
    stage.cues.event(&account_event("active", "online"));
    run.wait_for("online", |i| i["online"] == json!(true));
    let timing = StepTiming {
        count_wait: Duration::from_millis(300),
        ..short()
    };
    let end = call_step(&stage, "sync", &[], timing, &|_| {});
    assert_eq!(end.status, 1, "a blind sync passed");
    assert!(
        end.reason.contains("no count line came") && end.reason.contains("judge nothing"),
        "{}",
        end.reason
    );
    assert!(
        end.notes.iter().any(|n| n.contains("not measured")),
        "{:?}",
        end.notes
    );
    assert_eq!(stage.records()[0]["result"], json!("FAIL"));
    // With one count line it passes, as before.
    stage.cues.line(COUNT_ZERO);
    run.wait_for("a count line", |i| {
        i["lines"]["L_COUNT"].as_u64() >= Some(1)
    });
    let end = call_step(&stage, "sync", &[], short(), &|_| {});
    assert_eq!(end.status, 0, "{}", end.reason);
    finish(&stage, run);
}

#[test]
fn a_step_that_needs_a_link_refuses_on_a_profile_without_one() {
    let stage = Stage::new("t6-sync-unlinked");
    let run = stage.start(false);
    for name in ["sync", "reconnect"] {
        let end = call_step(&stage, name, &[], short(), &|_| {});
        assert_eq!(end.status, 1, "{name}");
        assert!(end.reason.contains("not linked"), "{name}: {}", end.reason);
    }
    finish(&stage, run);
}

// ---- receive, group-receive ------------------------------------------------------------------------------

fn online(stage: &Stage, run: &mut Running) {
    stage.cues.event(&account_event("active", "online"));
    run.wait_for("online", |i| i["online"] == json!(true));
}

#[test]
fn receive_and_group_receive_see_one_new_text_once_and_open_no_chat() {
    for (name, kind, id) in [("receive", "direct", 1), ("group-receive", "group", 2)] {
        let stage = Stage::new(&format!("t6-{name}"));
        stage.linked();
        stage.cues.chats(&[
            chat(1, "direct", 0, 1000, "none"),
            chat(2, "group", 0, 900, "none"),
        ]);
        let mut run = stage.start(false);
        online(&stage, &mut run);
        let end = drive(&stage, name, &[], short(), &[], move |cues, hear| {
            hear.until("PHONE:");
            cues.chats(&[
                chat(1, "direct", i64::from(id == 1), 2000, "none"),
                chat(2, "group", i64::from(id == 2), 1900, "none"),
            ]);
        });
        assert_eq!(end.status, 0, "{name}: {}", end.reason);
        assert!(
            end.notes.iter().any(|n| n.contains(&format!(
                "one new message, once, in {kind} conversation #{id}"
            ))),
            "{name}: {:?}",
            end.notes
        );
        // Only `chats` and `state` were asked of the app: no chat was opened, nothing sent.
        assert!(
            stage.seen().iter().all(|c| c == "chats" || c == "state"),
            "{name}: {:?}",
            stage.seen()
        );
        let facts = &stage.records()[0]["facts"]["chats"];
        assert_eq!(facts[0]["chat"], json!(id));
        assert_eq!(facts[0]["unread"], json!(1));
        nothing_of_a_value(&end, &stage);
        finish(&stage, run);
    }
}

#[test]
fn receive_s7_twice_and_lost_halt_the_run() {
    // Two new messages in one chat where one was sent.
    let stage = Stage::new("t6-twice");
    stage.linked();
    stage.cues.chats(&[chat(1, "direct", 0, 1000, "none")]);
    let mut run = stage.start(false);
    online(&stage, &mut run);
    let end = drive(&stage, "receive", &[], short(), &[], |cues, hear| {
        hear.until("PHONE:");
        cues.chats(&[chat(1, "direct", 2, 2000, "none")]);
    });
    assert_eq!(end.status, 1);
    assert!(end.reason.contains("TWICE"), "{}", end.reason);
    finish_halted(run, "twice", &stage);

    // Nothing arrives within the bound: counted lost.
    let stage = Stage::new("t6-lost");
    stage.linked();
    stage.cues.chats(&[chat(1, "direct", 0, 1000, "none")]);
    let mut run = stage.start(false);
    online(&stage, &mut run);
    let timing = StepTiming {
        lost_after: Duration::from_millis(500),
        ..short()
    };
    let end = drive(&stage, "receive", &[], timing, &[], |_, _| {});
    assert_eq!(end.status, 1);
    assert!(end.reason.contains("counted LOST"), "{}", end.reason);
    finish_halted(run, "lost", &stage);
}

#[test]
fn receive_resumes_from_its_baseline_and_a_text_between_the_calls_counts() {
    let stage = Stage::new("t6-resume");
    stage.linked();
    stage.cues.chats(&[chat(1, "direct", 0, 1000, "none")]);
    let mut run = stage.start(false);
    online(&stage, &mut run);
    // The first call runs out of its own time (not of the text's 10 minutes).
    let timing = StepTiming {
        call_max: Duration::from_millis(700),
        ..short()
    };
    let first = call_step(&stage, "receive", &[], timing, &|_| {});
    assert_eq!(first.status, 1);
    assert!(
        first.reason.contains("run the step again"),
        "{}",
        first.reason
    );
    let record = stage.records().pop().expect("a record");
    assert_eq!(record["result"], json!("FAIL"));
    assert!(record["resume"]["base"].is_object(), "{record}");
    // The text comes between the calls; the second call keeps the baseline and sees it.
    stage.cues.chats(&[chat(1, "direct", 1, 2000, "none")]);
    let second = call_step(&stage, "receive", &[], short(), &|_| {});
    assert_eq!(second.status, 0, "{}", second.reason);
    assert!(
        second
            .notes
            .iter()
            .any(|n| n.contains("baseline of the earlier call"))
    );
    finish(&stage, run);
}

#[test]
fn receive_a_text_that_opens_a_chat_nobody_listed_is_a_finding() {
    let stage = Stage::new("t6-fresh");
    stage.linked();
    stage.cues.chats(&[chat(1, "direct", 0, 1000, "none")]);
    let mut run = stage.start(false);
    online(&stage, &mut run);
    let end = drive(&stage, "receive", &[], short(), &[], |cues, hear| {
        hear.until("PHONE:");
        cues.chats(&[
            chat(1, "direct", 0, 1000, "none"),
            chat(9, "direct", 1, 2000, "none"),
        ]);
    });
    assert_eq!(end.status, 1);
    assert!(
        end.reason.contains("did not exist before"),
        "{}",
        end.reason
    );
    finish(&stage, run);
}

// ---- mirror -----------------------------------------------------------------------------------------------------

#[test]
fn mirror_sees_one_own_message_sent_and_then_read() {
    let stage = Stage::new("t6-mirror");
    stage.linked();
    stage.cues.chats(&[
        chat(7, "direct", 1, 1000, "none"),
        chat(8, "group", 0, 500, "none"),
    ]);
    let mut run = stage.start(false);
    online(&stage, &mut run);
    let answers = [("pp-read-it", true), ("phone-warning", false)];
    let end = drive(&stage, "mirror", &[], short(), &answers, |cues, hear| {
        hear.until("PHONE:");
        cues.chats(&[
            chat(7, "direct", 1, 2000, "sent"),
            chat(8, "group", 0, 500, "none"),
        ]);
        hear.until("TERMINAL:");
        cues.chats(&[
            chat(7, "direct", 1, 2000, "read"),
            chat(8, "group", 0, 500, "none"),
        ]);
    });
    assert_eq!(end.status, 0, "{}", end.reason);
    let notes = end.notes.join("\n");
    assert!(notes.contains("read from the list"), "{notes}");
    assert!(notes.contains("receipt shows `sent`"), "{notes}");
    assert!(notes.contains("reached `read`"), "{notes}");
    // The step opened no chat (opening one with unread messages would mark it read).
    assert!(
        stage.seen().iter().all(|c| c == "chats" || c == "state"),
        "{:?}",
        stage.seen()
    );
    nothing_of_a_value(&end, &stage);
    finish(&stage, run);
}

/// A count line of the socket that stands now, with the frames of a send.
fn count_with_a_send(reflects: u64, boxes: u64) -> String {
    COUNT_ZERO
        .replace(
            "out_reflect_transmit=0",
            &format!("out_reflect_transmit={reflects}"),
        )
        .replace("out_box_transmit=0", &format!("out_box_transmit={boxes}"))
}

fn info_at(socket: &std::path::Path) -> Value {
    call_at(socket, &json!({"op": "info"}), Duration::from_secs(5)).expect("the info")
}

#[test]
fn send_opens_its_window_before_the_user_is_told_and_closes_it_at_the_end() {
    let stage = Stage::new("th8-send");
    stage.linked();
    stage.cues.chats(&[
        chat(7, "direct", 0, 1000, "none"),
        chat(8, "group", 0, 500, "none"),
    ]);
    let mut run = stage.start(false);
    online(&stage, &mut run);
    let socket = stage.rig.socket();
    assert_eq!(info_at(&socket)["send_window"], json!(false));
    let answers = [
        ("pp-got-send", true),
        ("tp-mirrors-send", true),
        ("phone-warning", false),
    ];
    let at = socket.clone();
    let end = drive(&stage, "send", &[], short(), &answers, move |cues, hear| {
        hear.until("WINDOW:");
        assert_eq!(info_at(&at)["send_window"], json!(true));
        cues.line(&count_with_a_send(1, 1));
        cues.chats(&[
            chat(7, "direct", 0, 2000, "sent"),
            chat(8, "group", 0, 500, "none"),
        ]);
    });
    assert_eq!(end.status, 0, "{}", end.reason);
    let notes = end.notes.join("\n");
    assert!(notes.contains("reflects 1, boxes 1"), "{notes}");
    let info = info_at(&socket);
    assert_eq!(info["send_window"], json!(false), "{info}");
    assert_eq!(info["halted"], Value::Null, "{info}");
    // The step's frames stay allowed, one more is a send nobody asked for.
    stage.cues.line(&count_with_a_send(1, 1));
    stage.cues.line(&count_with_a_send(1, 2));
    finish_halted(run, "transmit", &stage);
}

#[test]
fn group_send_watches_the_group_chat_only() {
    let stage = Stage::new("th8-group-send");
    stage.linked();
    stage.cues.chats(&[
        chat(7, "direct", 0, 1000, "none"),
        chat(8, "group", 0, 500, "none"),
    ]);
    let mut run = stage.start(false);
    online(&stage, &mut run);
    let answers = [
        ("pp-got-group-send", true),
        ("tp-mirrors-group-send", true),
        ("phone-warning", false),
    ];
    let end = drive(
        &stage,
        "group-send",
        &[],
        short(),
        &answers,
        |cues, hear| {
            hear.until("WINDOW:");
            // Something new in the 1:1 chat is not the send.
            cues.chats(&[
                chat(7, "direct", 0, 3000, "sent"),
                chat(8, "group", 0, 500, "none"),
            ]);
            std::thread::sleep(Duration::from_millis(200));
            cues.line(&count_with_a_send(1, 2));
            cues.chats(&[
                chat(7, "direct", 0, 3000, "sent"),
                chat(8, "group", 0, 900, "sent"),
            ]);
        },
    );
    assert_eq!(end.status, 0, "{}", end.reason);
    assert!(end.notes.join("\n").contains("boxes 2"));
    finish(&stage, run);
}

#[test]
fn a_send_the_personal_phone_did_not_show_once_halts_the_run() {
    let stage = Stage::new("th8-send-lost");
    stage.linked();
    stage.cues.chats(&[chat(7, "direct", 0, 1000, "none")]);
    let mut run = stage.start(false);
    online(&stage, &mut run);
    let answers = [("pp-got-send", false)];
    let end = drive(&stage, "send", &[], short(), &answers, |cues, hear| {
        hear.until("WINDOW:");
        cues.line(&count_with_a_send(1, 1));
        cues.chats(&[chat(7, "direct", 0, 2000, "sent")]);
    });
    assert_ne!(end.status, 0);
    assert!(end.reason.contains("exactly once"), "{}", end.reason);
    finish_halted(run, "lost", &stage);
}

#[test]
fn a_send_that_shows_failed_is_a_finding() {
    let stage = Stage::new("th8-send-failed");
    stage.linked();
    stage.cues.chats(&[chat(7, "direct", 0, 1000, "none")]);
    let mut run = stage.start(false);
    online(&stage, &mut run);
    let socket = stage.rig.socket();
    let end = drive(&stage, "send", &[], short(), &[], |cues, hear| {
        hear.until("WINDOW:");
        cues.chats(&[chat(7, "direct", 0, 2000, "failed")]);
    });
    assert_ne!(end.status, 0);
    assert!(end.reason.contains("failed"), "{}", end.reason);
    assert_eq!(info_at(&socket)["send_window"], json!(false));
    finish(&stage, run);
}

#[test]
fn a_call_that_stops_short_keeps_the_window_and_never_asks_for_a_second_send() {
    let stage = Stage::new("th8-send-resume");
    stage.linked();
    stage.cues.chats(&[chat(7, "direct", 0, 1000, "none")]);
    let mut run = stage.start(false);
    online(&stage, &mut run);
    let socket = stage.rig.socket();
    // The send does not show in this call.
    let end = drive(&stage, "send", &["--seconds", "1"], short(), &[], |_, _| {});
    assert_ne!(end.status, 0);
    assert_eq!(info_at(&socket)["send_window"], json!(true));
    assert!(said(&end, "WINDOW:"), "{:?}", end.said);
    // The next call goes on from the first one's chat state and does not tell the user to send.
    let answers = [
        ("pp-got-send", true),
        ("tp-mirrors-send", true),
        ("phone-warning", false),
    ];
    stage.cues.line(&count_with_a_send(1, 1));
    stage.cues.chats(&[chat(7, "direct", 0, 2000, "sent")]);
    let end = drive(&stage, "send", &[], short(), &answers, |_, _| {});
    assert_eq!(end.status, 0, "{}", end.reason);
    assert!(!said(&end, "WINDOW:"), "{:?}", end.said);
    assert_eq!(info_at(&socket)["send_window"], json!(false));
    finish(&stage, run);
}

/// The `send` and `attach` commands the app got, in order, whole.
fn asked(stage: &Stage) -> Vec<Value> {
    std::fs::read_to_string(stage.cues.0.join("asked"))
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str(l).expect("a command"))
        .collect()
}

/// The app shows this chat open, with one looked-at file in its composer.
fn open_in_the_app(stage: &Stage, id: i64) {
    stage.cues.put(
        "state.json",
        &json!({"open": id, "rows": [], "composer": {
            "planned": true, "preparing": 0, "files": 1, "sendable": 1, "refused": 0
        }}),
    );
}

#[test]
fn a_lead_send_opens_the_chat_and_sends_the_steps_marker_once() {
    let stage = Stage::new("th8-lead-send");
    stage.linked();
    stage.cues.chats(&[
        chat(7, "direct", 0, 1000, "none"),
        chat(8, "group", 0, 500, "none"),
    ]);
    open_in_the_app(&stage, 8);
    let mut run = stage.start(false);
    online(&stage, &mut run);
    let answers = [
        ("pp-got-group-send", true),
        ("tp-mirrors-group-send", true),
        ("phone-warning", false),
    ];
    let end = drive(
        &stage,
        "group-send",
        &["--lead-sends"],
        short(),
        &answers,
        |cues, hear| {
            hear.until("the tool's send to show");
            cues.line(&count_with_a_send(1, 1));
            cues.chats(&[
                chat(7, "direct", 0, 1000, "none"),
                chat(8, "group", 0, 900, "sent"),
            ]);
        },
    );
    assert_eq!(end.status, 0, "{}", end.reason);
    assert!(!said(&end, "WINDOW:"), "{:?}", end.said);
    let commands = asked(&stage);
    assert_eq!(commands.len(), 1, "{commands:?}");
    assert_eq!(commands[0]["cmd"], json!("send"));
    let args = &commands[0]["args"];
    assert_eq!(args["conversation"], json!(8));
    assert_eq!(args["files"], json!(0));
    assert!(
        args["text"]
            .as_str()
            .is_some_and(|t| t.starts_with("kourier-rt1-group-send-")),
        "{args}"
    );
    let seen = stage.seen();
    let place = |c: &str| seen.iter().position(|s| s == c).unwrap_or(usize::MAX);
    assert!(place("open_id") < place("send"), "{seen:?}");
    finish(&stage, run);
}

#[test]
fn a_lead_send_file_attaches_the_tools_own_picture_and_names_it() {
    let stage = Stage::new("th8-lead-file");
    stage.linked();
    stage.cues.chats(&[chat(7, "direct", 0, 1000, "none")]);
    open_in_the_app(&stage, 7);
    let mut run = stage.start(false);
    online(&stage, &mut run);
    let answers = [
        ("pp-got-file", true),
        ("tp-mirrors-file", true),
        ("phone-warning", false),
    ];
    let end = drive(
        &stage,
        "send-file",
        &["--lead-sends"],
        short(),
        &answers,
        |cues, hear| {
            hear.until("the tool's send to show");
            cues.line(&count_with_a_send(1, 1));
            cues.chats(&[chat(7, "direct", 0, 2000, "sent")]);
        },
    );
    assert_eq!(end.status, 0, "{}", end.reason);
    let commands = asked(&stage);
    assert_eq!(commands.len(), 2, "{commands:?}");
    assert_eq!(commands[0]["cmd"], json!("attach"));
    let path = commands[0]["args"]["path"].as_str().expect("a path");
    assert!(
        std::fs::read(path)
            .expect("the picture")
            .starts_with(b"\x89PNG"),
        "{path}"
    );
    assert_eq!(commands[1]["cmd"], json!("send"));
    assert_eq!(commands[1]["args"]["files"], json!(1));
    finish(&stage, run);
}

#[test]
fn a_lead_send_sends_nothing_when_its_kind_has_two_chats() {
    let stage = Stage::new("th8-lead-two");
    stage.linked();
    stage.cues.chats(&[
        chat(7, "direct", 0, 1000, "none"),
        chat(9, "direct", 0, 2000, "none"),
    ]);
    let mut run = stage.start(false);
    online(&stage, &mut run);
    let end = drive(&stage, "send", &["--lead-sends"], short(), &[], |_, _| {});
    assert_ne!(end.status, 0);
    assert!(end.reason.contains("more than one"), "{}", end.reason);
    assert!(asked(&stage).is_empty());
    finish(&stage, run);
}

#[test]
fn a_lead_send_whose_call_stops_short_is_not_made_again() {
    let stage = Stage::new("th8-lead-resume");
    stage.linked();
    stage.cues.chats(&[chat(7, "direct", 0, 1000, "none")]);
    open_in_the_app(&stage, 7);
    let mut run = stage.start(false);
    online(&stage, &mut run);
    let end = drive(
        &stage,
        "send",
        &["--lead-sends", "--seconds", "1"],
        short(),
        &[],
        |_, _| {},
    );
    assert_ne!(end.status, 0);
    assert_eq!(asked(&stage).len(), 1);
    let answers = [
        ("pp-got-send", true),
        ("tp-mirrors-send", true),
        ("phone-warning", false),
    ];
    stage.cues.line(&count_with_a_send(1, 1));
    stage.cues.chats(&[chat(7, "direct", 0, 2000, "sent")]);
    let end = drive(
        &stage,
        "send",
        &["--lead-sends"],
        short(),
        &answers,
        |_, _| {},
    );
    assert_eq!(end.status, 0, "{}", end.reason);
    assert_eq!(asked(&stage).len(), 1, "one send over both calls");
    finish(&stage, run);
}

#[test]
fn a_send_step_called_again_after_the_personal_yes_does_not_wait_for_the_preview() {
    let stage = Stage::new("th8-lead-preview");
    stage.linked();
    stage.cues.chats(&[chat(8, "group", 0, 500, "none")]);
    open_in_the_app(&stage, 8);
    let mut run = stage.start(false);
    online(&stage, &mut run);
    let socket = stage.rig.socket();
    let timing = StepTiming {
        answer_wait: Duration::from_millis(400),
        ..short()
    };
    let end = drive(
        &stage,
        "group-send",
        &["--lead-sends"],
        timing,
        &[("pp-got-group-send", true)],
        |cues, hear| {
            hear.until("the tool's send to show");
            cues.line(&count_with_a_send(1, 1));
            cues.chats(&[chat(8, "group", 0, 900, "sent")]);
        },
    );
    assert_ne!(end.status, 0, "the TEST phone's question stays open");
    assert_eq!(info_at(&socket)["send_window"], json!(true));
    // A member writes into the group: the preview is no longer the step's own message.
    stage.cues.chats(&[chat(8, "group", 0, 1200, "none")]);
    let answers = [("tp-mirrors-group-send", true), ("phone-warning", false)];
    let end = drive(
        &stage,
        "group-send",
        &["--lead-sends"],
        short(),
        &answers,
        |_, _| {},
    );
    assert_eq!(end.status, 0, "{}", end.reason);
    assert_eq!(asked(&stage).len(), 1, "one send over both calls");
    assert_eq!(info_at(&socket)["send_window"], json!(false));
    finish(&stage, run);
}

#[test]
fn a_steps_own_send_needs_its_window_and_its_marker_and_goes_once() {
    let stage = Stage::new("th8-step-send");
    stage.linked();
    stage.cues.chats(&[chat(7, "direct", 0, 1000, "none")]);
    open_in_the_app(&stage, 7);
    let mut run = stage.start(false);
    online(&stage, &mut run);
    let socket = stage.rig.socket();
    let op = |what: &str, text: &str, conversation: i64, picture: bool| json!({"op": "step_send", "what": what, "text": text, "conversation": conversation, "picture": picture});
    let refused = |request: &Value, code: &str| {
        let e = call_at(&socket, request, Duration::from_secs(30)).expect_err("refused");
        assert!(format!("{e:#}").contains(code), "{e:#}");
    };
    let marker = "kourier-rt1-send-abcdef";
    refused(&op("send", marker, 7, false), "no-window");
    call_at(
        &socket,
        &json!({"op": "send_window", "what": "send"}),
        Duration::from_secs(5),
    )
    .expect("the window");
    for bad in [
        op("kinds", "kourier-rt1-kinds-abcdef", 7, false),
        op("send", "hello", 7, false),
        op("send", "kourier-rt1-group-send-abcdef", 7, false),
        op("send", "kourier-rt1-send-", 7, false),
        op("send", "kourier-rt1-send-ABCDEF", 7, false),
        op("send", marker, 0, false),
        op("send", marker, 7, true),
    ] {
        refused(&bad, "badRequest");
    }
    assert!(asked(&stage).is_empty());
    call_at(
        &socket,
        &op("send", marker, 7, false),
        Duration::from_secs(30),
    )
    .expect("sent");
    refused(&op("send", marker, 7, false), "sent-before");
    assert_eq!(asked(&stage).len(), 1);
    call_at(
        &socket,
        &json!({"op": "send_window_over", "what": "send"}),
        Duration::from_secs(5),
    )
    .expect("closed");
    finish(&stage, run);
}

#[test]
fn files_waits_for_three_messages_and_asks_what_opened() {
    let stage = Stage::new("th8-files");
    stage.linked();
    stage.cues.chats(&[chat(7, "direct", 1, 1000, "none")]);
    let mut run = stage.start(false);
    online(&stage, &mut run);
    let answers = [("files-open", true), ("avatar-shows", false)];
    let end = drive(&stage, "files", &[], short(), &answers, |cues, hear| {
        hear.until("PHONE:");
        cues.chats(&[chat(7, "direct", 3, 1500, "none")]);
        std::thread::sleep(Duration::from_millis(200));
        cues.chats(&[chat(7, "direct", 4, 2000, "none")]);
    });
    assert_eq!(end.status, 0, "{}", end.reason);
    assert!(said(&end, "WINDOW:"), "{:?}", end.said);
    assert!(end.notes.join("\n").contains("no profile picture"));
    // The step opened no chat and writes nothing.
    assert!(stage.seen().iter().all(|c| c == "chats" || c == "state"));
    finish(&stage, run);
}

#[test]
fn files_refuses_a_chat_that_is_open() {
    let stage = Stage::new("th8-files-open");
    stage.linked();
    stage.cues.chats(&[chat(7, "direct", 0, 1000, "none")]);
    stage
        .cues
        .put("state.json", &json!({"open": 7, "rows": []}));
    let mut run = stage.start(false);
    online(&stage, &mut run);
    let end = drive(&stage, "files", &[], short(), &[], |_, _| {});
    assert_ne!(end.status, 0);
    assert!(
        end.reason.contains("open in the Kourier window"),
        "{}",
        end.reason
    );
    finish(&stage, run);
}

#[test]
fn mirror_counts_the_rows_of_a_chat_he_opened_and_a_read_that_never_comes_is_a_finding() {
    let stage = Stage::new("t6-mirror-open");
    stage.linked();
    stage.cues.chats(&[chat(7, "direct", 0, 1000, "none")]);
    stage.cues.put(
        "state.json",
        &json!({"open": 7, "rows": [{"message": 1, "own": false}]}),
    );
    let mut run = stage.start(false);
    online(&stage, &mut run);
    let timing = StepTiming {
        read_wait: Duration::from_millis(300),
        ..short()
    };
    let answers = [("pp-read-it", true)];
    let end = drive(&stage, "mirror", &[], timing, &answers, |cues, hear| {
        hear.until("PHONE:");
        cues.put(
            "state.json",
            &json!({"open": 7, "rows": [{"message": 2, "own": true}, {"message": 1, "own": false}]}),
        );
        cues.chats(&[chat(7, "direct", 0, 2000, "sent")]);
    });
    assert_eq!(end.status, 1);
    assert!(
        end.notes
            .iter()
            .any(|n| n.contains("own rows grew by exactly one")),
        "{:?}",
        end.notes
    );
    assert!(end.reason.contains("did not read `read`"), "{}", end.reason);
    finish(&stage, run);

    // Two own rows where one was written: S7 twice.
    let stage = Stage::new("t6-mirror-twice");
    stage.linked();
    stage.cues.chats(&[chat(7, "direct", 0, 1000, "none")]);
    stage
        .cues
        .put("state.json", &json!({"open": 7, "rows": []}));
    let mut run = stage.start(false);
    online(&stage, &mut run);
    let end = drive(&stage, "mirror", &[], short(), &[], |cues, hear| {
        hear.until("PHONE:");
        cues.put(
            "state.json",
            &json!({"open": 7, "rows": [{"message": 3, "own": true}, {"message": 2, "own": true}]}),
        );
        cues.chats(&[chat(7, "direct", 0, 2000, "sent")]);
    });
    assert_eq!(end.status, 1);
    assert!(end.reason.contains("halted (twice)"), "{}", end.reason);
    finish_halted(run, "twice", &stage);
}

// ---- reconnect ---------------------------------------------------------------------------------------------------

fn earlier_step(stage: &Stage, unread: i64, activity: i64) {
    let record = json!({
        "name": "receive", "result": "PASS", "reason": "", "started_ms": 1, "ended_ms": 2,
        "notes": [], "resume": null,
        "facts": {"chats": [{"chat": 1, "unread": unread, "last_activity": activity}]},
    });
    std::fs::write(stage.rig.dir.steps_log(), format!("{record}\n")).expect("steps.jsonl");
}

#[test]
fn reconnect_is_online_without_a_login_and_the_chats_stand_as_they_were_left() {
    let stage = Stage::new("t6-reconnect");
    stage.linked();
    earlier_step(&stage, 1, 2000);
    stage.cues.chats(&[chat(1, "direct", 1, 2000, "none")]);
    let mut run = stage.start(false);
    online(&stage, &mut run);
    let end = call_step(&stage, "reconnect", &[], short(), &|_| {});
    assert_eq!(end.status, 0, "{}", end.reason);
    let notes = end.notes.join("\n");
    assert!(
        notes.contains("online again without a login step and without a 4115"),
        "{notes}"
    );
    assert!(notes.contains("does not lead"), "{notes}");
    assert!(
        notes.contains("1 chats of the earlier steps stand as they were left"),
        "{notes}"
    );
    finish(&stage, run);

    // A different unread count: a text lost or shown twice, or a later one: a FAIL with its words.
    let stage = Stage::new("t6-reconnect-moved");
    stage.linked();
    earlier_step(&stage, 1, 2000);
    stage.cues.chats(&[chat(1, "direct", 2, 2500, "none")]);
    let mut run = stage.start(false);
    online(&stage, &mut run);
    let end = call_step(&stage, "reconnect", &[], short(), &|_| {});
    assert_eq!(end.status, 1);
    assert!(
        end.reason.contains("do not stand as they were left"),
        "{}",
        end.reason
    );
    finish(&stage, run);
}

#[test]
fn reconnect_that_shows_a_login_step_is_not_a_reconnect() {
    let stage = Stage::new("t6-reconnect-login");
    stage.linked();
    let mut run = stage.start(false);
    stage.cues.event(&step_event("qr", "qr"));
    run.wait_for("a login step", |i| i["login_seen"] == json!(true));
    online(&stage, &mut run);
    let end = call_step(&stage, "reconnect", &[], short(), &|_| {});
    assert_eq!(end.status, 1);
    assert!(
        end.reason.contains("did not simply reconnect"),
        "{}",
        end.reason
    );
    finish(&stage, run);
}

// ---- passive -------------------------------------------------------------------------------------------------------

#[test]
fn passive_watches_for_its_seconds_and_goes_on_in_a_second_call() {
    let stage = Stage::new("t6-passive");
    stage.linked();
    let mut run = stage.start(false);
    online(&stage, &mut run);
    // One call is enough; a count line comes while it watches.
    let end = drive(
        &stage,
        "passive",
        &["--seconds", "1"],
        short(),
        &[],
        |cues, hear| {
            hear.until("WAIT:");
            cues.line(COUNT_ZERO);
        },
    );
    assert_eq!(end.status, 0, "{}", end.reason);
    let notes = end.notes.join("\n");
    assert!(notes.contains("online at each of"), "{notes}");
    assert!(notes.contains("follower words only"), "{notes}");
    assert!(notes.contains("last count line: when=running"), "{notes}");
    // A call that is shorter than the seconds ends FAIL with its resume; the next goes on, and the
    // last one judges the whole watch.
    let timing = StepTiming {
        call_max: Duration::from_millis(400),
        ..short()
    };
    let mut calls = 0;
    let end = loop {
        calls += 1;
        let end = call_step(&stage, "passive", &["--seconds", "1"], timing, &|_| {});
        if end.status == 0 {
            break end;
        }
        assert!(
            end.reason.contains("run `step passive --seconds 1` again"),
            "{}",
            end.reason
        );
        assert!(calls < 6, "it never finished");
    };
    assert!(calls >= 2, "{calls}");
    assert!(
        end.notes.iter().any(|n| n.contains("over 1 s")),
        "{:?}",
        end.notes
    );
    finish(&stage, run);
}

#[test]
fn passive_stops_at_a_halt_and_at_a_close() {
    // A halt while the step watches (S8: an ERROR line of the core).
    let stage = Stage::new("t6-passive-halt");
    stage.linked();
    let mut run = stage.start(false);
    online(&stage, &mut run);
    let timing = StepTiming {
        look: Duration::from_millis(50),
        ..short()
    };
    stage.cues.line(ERROR_LINE);
    run.wait_for("the halt", |i| i["halted"].is_string());
    let end = call_step(&stage, "passive", &["--seconds", "5"], timing, &|_| {});
    assert_eq!(end.status, 1);
    assert!(
        end.reason.contains("the run is halted (error)"),
        "{}",
        end.reason
    );
    stage.release();
    let _ = run.join_within(Duration::from_secs(30));

    // A close that is no halt (code 1000) is not passive.
    let stage = Stage::new("t6-passive-close");
    stage.linked();
    let mut run = stage.start(false);
    online(&stage, &mut run);
    stage.cues.line(E1_NORMAL);
    run.wait_for("the close", |i| i["lines"]["E1_CLOSED"].as_u64() >= Some(1));
    let end = call_step(&stage, "passive", &["--seconds", "1"], short(), &|_| {});
    // The close was before the step began: the watch starts at the line counter, so it is not seen.
    assert_eq!(end.status, 0, "{}", end.reason);
    let cues = stage.cues.clone();
    let mid = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(150));
        cues.line(E1_NORMAL);
    });
    let end = call_step(&stage, "passive", &["--seconds", "1"], short(), &|_| {});
    mid.join().expect("the cue");
    assert_eq!(end.status, 1);
    assert!(end.reason.contains("closed the socket"), "{}", end.reason);
    finish(&stage, run);
}

mod auto;
mod part2;

// ---- the table and the command line ----------------------------------------------------------------------

#[test]
fn the_steps_are_listed_in_order_and_an_unknown_name_is_never_echoed() {
    let list = list_steps();
    let names: Vec<&str> = STEPS.iter().map(|d| d.name).collect();
    assert_eq!(
        names,
        [
            "link",
            "sync",
            "receive",
            "group-receive",
            "mirror",
            "reconnect",
            "passive",
            "kinds",
            "files",
            "send",
            "group-send",
            "send-file",
            "phone-sleep",
            "flight-mode",
            "removed-on-phone",
            "unlink"
        ]
    );
    for name in &names {
        assert!(list.contains(name), "{name}");
    }
    assert!(!list.contains(&id_shape()) && !list.contains(&digest_shape()));
    // A step name with the shape of an ID (eight lower-case letters) passes the parser; the table
    // refuses it, and the sentence holds nothing of it.
    let lookalike = ["syn", "thtid"].concat();
    let a = args(&["step", &lookalike]).expect("the parser lets a lower-case name through");
    let e = step_command(&a).expect_err("an unknown step");
    let text = format!("{e:#}");
    assert!(text.contains("unknown step"), "{text}");
    assert!(!text.to_ascii_lowercase().contains(&lookalike), "{text}");
    // The names of the table are lower-case letters and dashes only (the parser's grammar).
    assert!(
        names
            .iter()
            .all(|n| n.bytes().all(|b| b.is_ascii_lowercase() || b == b'-'))
    );
    // Every question a step asks is one the supervisor takes.
    let asked = [
        "test-phone-in-hand",
        "emoji-equal",
        "list-shows-kourier",
        "menu-path-as-runbook",
        "phone-warning",
        "pp-read-it",
        "kinds-sent",
        "phone-locked",
        "tp-shows-once",
        "push-own",
        "flight-on",
        "pp-delivered",
        "flight-off-both",
        "tp-shows-later",
        "fs-line",
        "end-go",
        "tp-list-clean",
    ];
    for q in asked {
        assert!(QUESTIONS.iter().any(|(name, _)| *name == q), "{q}");
    }
}
