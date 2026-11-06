//! The supervisor (T3b): the redaction, the readers' collection, the four ends that write the switch
//! off (a stop, a crash, the halt of the first-answer time limit, a signal), the lock held until the
//! app is reaped, the wait for the ID, the forwarded commands, `state` and `stop`. The app is a shell
//! script that plays the dev channel on stdout and stdin and the log on stderr; nothing starts the
//! real app, nothing contacts a host, no window, no sound. Every wait is on an outcome (a file, an
//! answer, a thread's end), never on a measured time. No mutant.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde_json::{Value, json};

use super::preflight::current_uid;
use super::{World, digest_shape, error_of, id_shape};
use crate::threema_run::launch::{Order, Stamp, order_json, socket_path, take_profile_lock};
use crate::threema_run::lines::row_of;
use crate::threema_run::preflight::{LINES, Lock, app_lock_file, probe_lock};
use crate::threema_run::profile::{ProfileDir, State, open_lock_file, private_dir, read_json};
use crate::threema_run::supervise::{
    APP_LOG_CAP, LogSink, Refused, Seams, StopTiming, Timing, call_at, is_an_id_token, kind_word,
    redact, redact_value, state_in, stop_in, supervise_in, word,
};

pub(super) fn real_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

// ---- The redaction ----------------------------------------------------------------------------------

#[test]
fn the_redaction_masks_a_digest_and_an_id_shape_and_nothing_else() {
    let (digest, id) = (digest_shape(), id_shape());
    let gateway = ["*SYNTH", "01"].concat();
    let rows: Vec<(String, String)> = vec![
        // A run of 64 hex digits, bare, after glue, longer, in capitals, among words.
        (digest.clone(), "[digest]".into()),
        (format!("sha256:{digest}"), "sha256:[digest]".into()),
        (format!("{digest}00"), "[digest]".into()),
        (digest.to_uppercase(), "[digest]".into()),
        (format!("a {digest}, b"), "a [digest], b".into()),
        (format!("{digest} {digest}"), "[digest] [digest]".into()),
        // One digit short is no digest.
        ("9f".repeat(31) + "9", "9f".repeat(31) + "9"),
        // The shape of an ID, bare, in a field, a Gateway ID, twice, next to a multi-byte letter.
        (id.clone(), "[id]".into()),
        (format!("peer={id};"), "peer=[id];".into()),
        (format!("to {gateway} and {id}"), "to [id] and [id]".into()),
        (format!("\u{e9}{id}\u{e9}"), "\u{e9}[id]\u{e9}".into()),
        // RT1: the tool's OWN upper-case words of eight characters are no ID ("PERSONAL" became "[id]" in the
        // report); the same shape from outside still is.
        ("PERSONAL phone".into(), "PERSONAL phone".into()),
        (format!("PERSONAL {id}"), "PERSONAL [id]".into()),
        // What is not the shape: lower-case words of eight letters, a mixed-case kind, eight digits
        // (a counter), nine and seven characters, an underscore name, a count line.
        ("incoming messages".into(), "incoming messages".into()),
        ("kind=ServerAlert".into(), "kind=ServerAlert".into()),
        ("in_echo=12345678".into(), "in_echo=12345678".into()),
        (format!("{id}9"), format!("{id}9")),
        (id[..7].to_owned(), id[..7].to_owned()),
        ("STA_UNSYNC".into(), "STA_UNSYNC".into()),
        ("Removed1".into(), "Removed1".into()),
        (
            "socket frame counts when=running held_by=csp-unknown-type:1,group-sync-needed:2"
                .into(),
            "socket frame counts when=running held_by=csp-unknown-type:1,group-sync-needed:2"
                .into(),
        ),
    ];
    for (input, want) in rows {
        assert_eq!(redact(&input), want, "{input}");
    }
    // Both shapes are told apart from the hex of a digest: the ID shape inside a digest is the
    // digest's, and the whole run goes.
    assert_eq!(redact(&format!("{digest}{id}")), "[digest][id]");
    // The same in a JSON value: every string, nested; the keys are the protocol's words and stay.
    let value = json!({"kind": id, "list": [digest, {"x": format!("id {id}")}], "n": 7});
    assert_eq!(
        redact_value(&value),
        json!({"kind": "[id]", "list": ["[digest]", {"x": "id [id]"}], "n": 7})
    );
    assert!(is_an_id_token(&id) && !is_an_id_token("incoming"));
}

#[test]
fn a_word_of_the_protocol_is_a_word_or_other() {
    assert_eq!(word("NotTheExpectedAccount"), "NotTheExpectedAccount");
    assert_eq!(word("linking"), "linking");
    assert_eq!(word("a text with spaces"), "other");
    assert_eq!(word(""), "other");
    assert_eq!(word(&"x".repeat(41)), "other");
    assert_eq!(word(&id_shape()), "[id]");
    // The kind of an error is the part before the first colon; the detail never goes on.
    assert_eq!(
        kind_word(&format!(
            "NotTheExpectedAccount: the account is {}",
            id_shape()
        )),
        "NotTheExpectedAccount"
    );
    assert_eq!(kind_word("Pin Mismatch: x"), "Pin-Mismatch");
    assert_eq!(
        kind_word("no colon at all, just a long sentence of prose that is no word"),
        "other"
    );
}

#[test]
fn every_row_of_the_line_table_is_recognised_and_the_belts_too() {
    for row in LINES {
        let line = format!(
            "2026-11-04T10:00:00.000000Z  INFO kourier_threema: {} a=1",
            row.text
        );
        let want = crate::threema_run::lines::row_id(row);
        assert_eq!(row_of(&line), Some(want), "{}", row.text);
    }
    assert_eq!(
        row_of("2026-11-04T10:00:00Z ERROR kourier_core: x"),
        Some("ERROR_LINE")
    );
    assert_eq!(
        row_of("thread 'main' panicked at src/x.rs:1:1"),
        Some("PANIC")
    );
    assert_eq!(
        row_of("2026-11-04T10:00:00Z  INFO kourier_core: a plain line"),
        None
    );
}

#[test]
fn app_log_has_a_cap_and_a_big_old_log_is_kept_once() {
    let w = World::new("s-cap");
    let path = w.scratch.0.join("app.log");
    let mut sink = LogSink::open(&path, 100);
    for i in 0..30 {
        sink.line(&format!("line {i}"));
    }
    let text = std::fs::read_to_string(&path).expect("the log");
    assert!(text.starts_with("line 0\n"));
    assert_eq!(text.matches("reached its cap").count(), 1, "{text}");
    assert!(!text.contains("line 29"), "{text}");
    assert_eq!(
        std::fs::metadata(&path).expect("meta").permissions().mode() & 0o777,
        0o600
    );
    // A log of an earlier run past the cap becomes app.log.1 and a new one starts.
    let sink = LogSink::open(&path, 100);
    drop(sink);
    // (it was 100+ bytes: the first open already wrote past the cap, so this one rotated)
    assert!(path.with_extension("log.1").exists());
    assert!(std::fs::read_to_string(&path).expect("new").is_empty());
}

// ---- The stand-in app ---------------------------------------------------------------------------------

/// The app, as a shell script. The case (`STANDIN_CASE`) says what it does; the synthetic ID and
/// digest it prints are given in its environment (the test builds them from parts).
const STAND_IN: &str = r##"#!/bin/sh
case "$STANDIN_CASE" in
  silent) exec sleep 600 ;;
esac
say() { printf '%s\n' "$1"; }
say '{"type":"event","event":"channel","data":{"protocol":1}}'
echo "2026-11-04T10:00:00.000000Z  INFO kourier_threema::conn::lines: socket frame counts when=running in_reflected=0 in_reflect_ack=0 in_dry=0 in_promoted=0 in_drop_ack=0 in_tx=0 in_csp_message=0 in_message_ack=0 in_queue_complete=0 in_echo=0 in_close_error=0 in_other=0 out_reflected_ack=0 out_reflect_job=0 out_reflect_transmit=0 out_reflect_inbound=0 out_ephemeral=0 out_box_job=0 out_box_transmit=0 out_message_ack=0 out_unblock=0 out_tx=0 out_echo=0 out_idle=0 out_drop=0 out_other=0 held=0 held_by=none" >&2
echo "2026-11-04T10:00:01.000000Z  WARN kourier_threema::conn: the server's message was refused what=\"d2m\" unsupported=false peer=$STANDIN_ID" >&2
echo "2026-11-04T10:00:02.000000Z  INFO kourier_core: a plain line sha256:$STANDIN_DIGEST of $STANDIN_ID done" >&2
echo "2026-11-04T10:00:03.000000Z ERROR kourier_qt: something broke" >&2
BG=
case "$STANDIN_CASE" in
  link|link-crash|link-refused|link-wait|linking-halt)
    say '{"type":"event","event":"login_step","data":{"account":1,"step":"qr","kind":"qr"}}'
    say "{\"type\":\"event\",\"event\":\"login_qr\",\"data\":{\"account\":1,\"url\":\"threema://link?x=$STANDIN_DIGEST\"}}"
    say '{"type":"event","event":"login_step","data":{"account":1,"step":"linking","kind":"progress"}}'
    ;;
esac
case "$STANDIN_CASE" in
  link)
    say '{"type":"event","event":"login_step","data":{"account":1,"step":"done","kind":"done"}}'
    say '{"type":"event","event":"account_state","data":{"account":1,"network":"Threema","state":"active","conn":"online","device":2}}'
    ;;
  link-crash) cp "$STANDIN_CONFIG" "$STANDIN_MARK"; sleep 0.2; exit 3 ;;
  link-refused)
    say '{"type":"event","event":"login_step","data":{"account":1,"step":"linking","kind":"failed"}}'
    say "{\"type\":\"event\",\"event\":\"error\",\"data\":{\"source\":\"login\",\"account\":1,\"message\":\"NotTheExpectedAccount: the account is $STANDIN_ID\"}}"
    ;;
  crash) cp "$STANDIN_CONFIG" "$STANDIN_MARK"; sleep 0.2; exit 3 ;;
  deaf) exec sleep 600 ;;
  unasked)
    echo "2026-11-04T10:00:04.000000Z  WARN kourier_threema::conn: a chat server message came unasked; closing" >&2
    ;;
  qr-halt)
    say '{"type":"event","event":"login_step","data":{"account":1,"step":"qr","kind":"qr"}}'
    sleep 0.3
    echo "2026-11-04T10:00:04.000000Z  WARN kourier_threema::conn: a chat server message came unasked; closing" >&2
    ;;
  linking-halt)
    sleep 0.2
    echo "2026-11-04T10:00:04.000000Z  WARN kourier_threema::conn: the mediator did not register the joined device as new" >&2
    ( sleep 0.3
      say '{"type":"event","event":"login_step","data":{"account":1,"step":"done","kind":"done"}}' ) &
    BG=$!
    ;;
  s1b-drop|s1b-silent)
    say '{"type":"event","event":"login_step","data":{"account":1,"step":"qr","kind":"qr"}}'
    say '{"type":"event","event":"login_step","data":{"account":1,"step":"emoji","kind":"emoji"}}'
    say '{"type":"event","event":"login_step","data":{"account":1,"step":"linking","kind":"progress"}}'
    say '{"type":"event","event":"login_step","data":{"account":1,"step":"linking","kind":"failed"}}'
    say '{"type":"event","event":"error","data":{"source":"login","account":1,"message":"Internal: the core refused the first row"}}'
    if [ "$STANDIN_CASE" = s1b-drop ]; then
      ( sleep 0.4
        echo "2026-11-04T10:00:05.000000Z  INFO kourier_threema::conn::drop_own: the own device's drop on a socket of its own outcome=Removed" >&2 ) &
      BG=$!
    fi
    ;;
esac
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed 's/.*"id":\([0-9]*\).*/\1/')
  cmd=$(printf '%s' "$line" | sed 's/.*"cmd":"\([a-z_]*\)".*/\1/')
  printf '%s\n' "$cmd" >> "$STANDIN_SEEN"
  case "$cmd" in
    quit) [ -n "$BG" ] && kill "$BG" 2>/dev/null
      printf '{"type":"response","id":%s,"ok":true,"result":{}}\n' "$id"; cp "$STANDIN_CONFIG" "$STANDIN_MARK"; exit 0 ;;
    state) printf '{"type":"response","id":%s,"ok":true,"result":{"banner":"%s","digest":"%s"}}\n' "$id" "$STANDIN_ID" "$STANDIN_DIGEST" ;;
    *) printf '{"type":"response","id":%s,"ok":true,"result":{}}\n' "$id" ;;
  esac
done
cp "$STANDIN_CONFIG" "$STANDIN_MARK"
"##;

fn stamp() -> Stamp {
    Stamp {
        commit: Some("a1b2c3d4e5".to_owned()),
        clean: true,
        migrations: BTreeMap::from([
            ("core/0001_a.sql".to_owned(), "b1".to_owned()),
            ("core/0022_b.sql".to_owned(), "b2".to_owned()),
            ("outbox/0001_c.sql".to_owned(), "b3".to_owned()),
            ("threema/0001_d.sql".to_owned(), "b4".to_owned()),
        ]),
        sha256: "ab".repeat(32),
        build: Some("debug".to_owned()),
        size: 1,
        mtime_ns: 1,
        inode: 1,
    }
}

pub(super) fn quick() -> Timing {
    Timing {
        first_answer: Duration::from_secs(30),
        kill_grace: Duration::from_millis(300),
        id_wait: Duration::from_secs(30),
        poll: Duration::from_millis(10),
        drain: Duration::from_secs(2),
        log_cap: APP_LOG_CAP,
        link_end: Duration::from_secs(3),
        drop_wait: Duration::from_secs(5),
        // MEDIUM-1: out of the way of every test that stays online without a count line; the
        // watchdog's own tests shorten it.
        count_watch: Duration::from_secs(600),
    }
}

const ON: &str = "networks-on = [\"Threema\"]\n";

pub(super) struct Rig {
    pub w: World,
    pub dir: ProfileDir,
    run_dir: PathBuf,
    script: PathBuf,
    /// LOW-7: what the supervisor's look at the app's `/proc` files finds readable (nothing by
    /// default: the stand-in is no execute-only copy).
    exposed: bool,
    /// What the supervisor's own read of the TEST phone shows (B1): the scripted Threema phone; a
    /// test changes `own_id` and the like through `phone`.
    pub phone: Arc<std::sync::Mutex<crate::threema_run::tphone_fake::ThreemaFake>>,
}

impl Rig {
    pub fn new(name: &str) -> Self {
        let w = World::new(name);
        let dir = ProfileDir {
            dir: w.profile(),
            rehearsal: false,
        };
        private_dir(&dir.tool()).expect("the tool's directory");
        State::new(1).save(&dir).expect("the state file");
        std::fs::create_dir_all(dir.config_file().parent().expect("parent")).expect("config dir");
        std::fs::write(dir.config_file(), ON).expect("the switch");
        let run_dir = w.scratch.0.join("r");
        private_dir(&run_dir).expect("the socket's directory");
        let script = w.scratch.0.join("app.sh");
        std::fs::write(&script, STAND_IN).expect("the stand-in");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        Self {
            w,
            dir,
            run_dir,
            script,
            exposed: false,
            phone: Arc::new(std::sync::Mutex::new(
                crate::threema_run::tphone_fake::ThreemaFake {
                    own_id: id_shape(),
                    ..Default::default()
                },
            )),
        }
    }

    /// The supervisor finds the app's `/proc/<pid>/environ` readable (LOW-7).
    pub fn exposing(mut self) -> Self {
        self.exposed = true;
        self
    }

    pub fn socket(&self) -> PathBuf {
        socket_path(&self.run_dir, &self.dir.dir).expect("the socket's path")
    }

    /// The same place as a rehearsal's (the order and the profile agree about it).
    pub fn rehearsing(mut self) -> Self {
        self.dir.rehearsal = true;
        self
    }

    /// What the app saw of the config at its last moment, when it had one.
    pub fn mark(&self) -> PathBuf {
        self.w.scratch.0.join("mark")
    }

    /// The commands the app received, one a line.
    pub fn seen(&self) -> PathBuf {
        self.w.scratch.0.join("seen")
    }

    pub fn config(&self) -> String {
        std::fs::read_to_string(self.dir.config_file()).expect("kourier.toml")
    }

    pub fn write_order(&self, for_link: bool) {
        let order = Order {
            socket: self.socket(),
            rehearsal: self.dir.rehearsal,
            for_link,
        };
        std::fs::write(
            self.dir.launch_file(),
            format!("{:#}\n", order_json(&self.dir, &order, real_now())),
        )
        .expect("the order");
    }

    /// A list of test IDs in the keeper root, as a link start leaves one (a placeholder: no digest).
    pub fn write_list(&self) {
        std::fs::write(self.dir.allow_file(), "placeholder\n").expect("a list");
    }

    pub fn run(&self, case: &str, for_link: bool, timing: Timing) -> Running {
        let env = vec![
            ("STANDIN_CASE", case.to_owned()),
            (
                "STANDIN_CONFIG",
                self.dir.config_file().display().to_string(),
            ),
            ("STANDIN_MARK", self.mark().display().to_string()),
            ("STANDIN_SEEN", self.seen().display().to_string()),
            ("STANDIN_ID", id_shape()),
            ("STANDIN_DIGEST", digest_shape()),
        ];
        self.run_with(self.script.clone(), env, for_link, timing)
    }

    /// Another stand-in app (T6's steps script) under the same supervisor: `script` is its text,
    /// `env` what it is given.
    pub fn run_script(
        &self,
        script: &str,
        env: Vec<(&'static str, String)>,
        for_link: bool,
        timing: Timing,
    ) -> Running {
        let path = self.w.scratch.0.join("other-app.sh");
        std::fs::write(&path, script).expect("the stand-in");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        self.run_with(path, env, for_link, timing)
    }

    fn run_with(
        &self,
        script: PathBuf,
        env: Vec<(&'static str, String)>,
        for_link: bool,
        timing: Timing,
    ) -> Running {
        self.write_order(for_link);
        let terminated = Arc::new(AtomicBool::new(false));
        let (dir, flag, exposed) = (self.dir.clone(), Arc::clone(&terminated), self.exposed);
        let scripted = Arc::clone(&self.phone);
        let handle = std::thread::spawn(move || {
            let make = move |_order: &Order| -> Result<Command> {
                let mut cmd = Command::new("/bin/sh");
                cmd.arg(&script);
                for (k, v) in &env {
                    cmd.env(k, v);
                }
                Ok(cmd)
            };
            supervise_in(
                &dir,
                &Seams {
                    uid: current_uid(),
                    now: &real_now,
                    check: &|_| Ok(stamp()),
                    make: &make,
                    timing: &timing,
                    terminated: &flag,
                    readable: &|_| exposed,
                    phone: Arc::new(move |_dir, _serial| {
                        let fake = scripted.lock().expect("the scripted phone").clone();
                        Ok(Box::new(fake.build()))
                    }),
                },
            )
        });
        Running {
            handle: Some(handle),
            terminated,
            socket: self.socket(),
        }
    }

    /// The text of the profile's files the tool writes: nothing of them may hold a value.
    pub fn written_text(&self) -> String {
        [
            self.dir.app_log(),
            self.dir.events_log(),
            self.dir.state_file(),
            self.dir.supervisor_file(),
            self.dir.config_file(),
        ]
        .iter()
        .map(|p| std::fs::read_to_string(p).unwrap_or_default())
        .collect::<Vec<_>>()
        .join("\n")
    }

    pub fn state(&self) -> Value {
        read_json(&self.dir.state_file())
            .expect("readable")
            .expect("a state file")
    }

    /// After every end: the switch off, the lock free, no socket, no `supervisor.json`.
    pub fn assert_ended(&self, what: &str) {
        assert_eq!(self.config(), "networks-on = []\n", "{what}: the switch");
        assert_eq!(
            probe_lock(&self.dir.lock_file(), Duration::ZERO).expect("a probe"),
            Lock::Free,
            "{what}: the lock"
        );
        assert!(!self.socket().exists(), "{what}: the socket");
        assert!(
            !self.dir.supervisor_file().exists(),
            "{what}: supervisor.json"
        );
    }

    pub fn app_saw_the_switch_on(&self) -> bool {
        std::fs::read_to_string(self.mark()).is_ok_and(|text| text == ON)
    }

    pub fn seen_commands(&self) -> Vec<String> {
        std::fs::read_to_string(self.seen())
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

pub(super) struct Running {
    handle: Option<JoinHandle<Result<i32>>>,
    terminated: Arc<AtomicBool>,
    socket: PathBuf,
}

impl Running {
    pub fn call(&self, request: &Value) -> Result<Value> {
        call_at(&self.socket, request, Duration::from_secs(10))
    }

    pub fn info(&self) -> Option<Value> {
        self.call(&json!({"op": "info"})).ok()
    }

    /// Polls `info` until `done` holds; the thread ending first is a failure with its result.
    pub fn wait_for(&mut self, what: &str, done: impl Fn(&Value) -> bool) -> Value {
        let limit = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(info) = self.info()
                && done(&info)
            {
                return info;
            }
            if self.handle.as_ref().is_some_and(|h| h.is_finished()) {
                let result = self.handle.take().expect("a handle").join();
                panic!("the supervisor ended before `{what}`: {result:?}");
            }
            assert!(Instant::now() < limit, "gave up waiting for `{what}`");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    pub fn ready(&mut self) -> Value {
        self.wait_for("ready", |i| i["ready"] == json!(true))
    }

    pub fn stop(&self) {
        self.call(&json!({"op": "stop"})).expect("stop is taken");
    }

    /// The supervisor's end within `bound`, or `None` when it still runs then (it is left running:
    /// the test's process takes it and the stand-in with it).
    pub fn join_within(self, bound: Duration) -> Option<Result<i32>> {
        let limit = Instant::now() + bound;
        while !self.handle.as_ref().is_some_and(|h| h.is_finished()) {
            if Instant::now() >= limit {
                return None;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        Some(self.join())
    }

    pub fn join(mut self) -> Result<i32> {
        self.handle
            .take()
            .expect("a handle")
            .join()
            .expect("the supervisor did not panic")
    }
}

fn pid_exists(pid: u64) -> bool {
    let pid = libc::pid_t::try_from(pid).expect("a pid");
    // SAFETY: signal 0 only asks whether the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

// ---- The readers collect --------------------------------------------------------------------------------

/// The stderr reader keeps the lines of the contract (and rule S8's two belts) raw in memory and
/// serves them redacted; the dev channel reader keeps events sanitized. Nothing of a value reaches a
/// file the tool writes or an answer of the socket.
#[test]
fn the_readers_collect_and_no_value_reaches_a_record() {
    let rig = Rig::new("s-read");
    let mut run = rig.run("normal", false, quick());
    run.ready();
    // The stand-in wrote its four lines before its channel was read: wait for the last.
    run.wait_for("four lines", |i| i["lines_total"].as_u64() >= Some(4));
    let lines = run
        .call(&json!({"op": "lines", "since": 0}))
        .expect("lines");
    let rows: Vec<&str> = lines["lines"]
        .as_array()
        .expect("lines")
        .iter()
        .map(|l| l["row"].as_str().expect("a row"))
        .collect();
    assert_eq!(rows, ["L_COUNT", "E4_REFUSED", "ERROR_LINE"], "{lines}");
    // The line that held the ID shape is served redacted; the kept raw line is in memory only.
    let served = lines.to_string();
    assert!(served.contains("peer=[id]"), "{served}");
    assert!(!served.contains(&id_shape()) && !served.contains(&digest_shape()));
    // `since` filters by sequence number.
    let later = run
        .call(&json!({"op": "lines", "since": 2}))
        .expect("lines");
    assert_eq!(later["lines"].as_array().expect("lines").len(), 1);
    let info = run.info().expect("info");
    assert_eq!(
        info["lines"],
        json!({"E4_REFUSED": 1, "ERROR_LINE": 1, "L_COUNT": 1})
    );
    assert_eq!(info["lines_total"], json!(4));
    let events = run
        .call(&json!({"op": "events", "since": 0}))
        .expect("events");
    assert_eq!(events["events"][0]["event"], json!("app_started"));
    assert_eq!(events["events"][1]["event"], json!("channel"));
    run.stop();
    assert_eq!(run.join().expect("ended"), 0);
    // Every record the tool wrote: no value in it, the masks in the app's log.
    let written = rig.written_text();
    assert!(!written.contains(&id_shape()), "an ID shape reached a file");
    assert!(
        !written.contains(&digest_shape()),
        "a digest reached a file"
    );
    let log = std::fs::read_to_string(rig.dir.app_log()).expect("app.log");
    assert!(log.contains("sha256:[digest] of [id] done"), "{log}");
    assert!(log.contains("peer=[id]"), "{log}");
    assert!(log.contains("==== threema-run: app started"), "{log}");
    assert!(log.contains("==== threema-run: app exited"), "{log}");
    rig.assert_ended("a stop");
}

/// The login's events are sanitized: the QR link of the app is dropped (only `shown` stays), an
/// error keeps its kind word only, the steps keep their words; `link_at` is written at `linking`
/// and cleared at `done`, the migrations that ran are recorded once the databases are open, and the
/// core's schema goes with the link.
#[test]
fn a_link_is_recorded_in_words_and_the_state_follows_it() {
    let rig = Rig::new("s-link");
    let mut run = rig.run("link", true, quick());
    run.wait_for("the wait for the ID", |i| i["id"] == json!("waiting"));
    run.call(&json!({"op": "test_id_arrived"}))
        .expect("the gate");
    run.wait_for("online", |i| i["online"] == json!(true));
    let info = run.info().expect("info");
    assert_eq!(info["login"], json!({"step": "done", "kind": "done"}));
    assert_eq!(
        info["account"],
        json!({"state": "active", "conn": "online", "device": 2})
    );
    assert_eq!(info["link_at"], json!(false), "cleared at done");
    let events = run
        .call(&json!({"op": "events", "since": 0}))
        .expect("events");
    let names: Vec<(&str, &Value)> = events["events"]
        .as_array()
        .expect("events")
        .iter()
        .map(|e| (e["event"].as_str().expect("a name"), &e["data"]))
        .collect();
    let qr = names
        .iter()
        .find(|(n, _)| *n == "login_qr")
        .expect("login_qr");
    assert_eq!(qr.1, &json!({"shown": true}), "the link is never kept");
    let step = names
        .iter()
        .filter(|(n, _)| *n == "login_step")
        .map(|(_, d)| (*d).clone())
        .collect::<Vec<_>>();
    assert_eq!(
        step,
        [
            json!({"step": "qr", "kind": "qr"}),
            json!({"step": "linking", "kind": "progress"}),
            json!({"step": "done", "kind": "done"}),
        ]
    );
    run.stop();
    run.join().expect("ended");
    let state = rig.state();
    assert_eq!(state["link_at"], Value::Null);
    assert!(state["linked_since_ms"].is_u64(), "{state}");
    assert_eq!(state["core_schema_at_link"], json!(22));
    // The core's files at the channel, the outbox's and Threema's once a login opened the backend.
    assert_eq!(
        state["migrations"],
        json!({"core/0001_a.sql": "b1", "core/0022_b.sql": "b2", "outbox/0001_c.sql": "b3", "threema/0001_d.sql": "b4"})
    );
    assert!(
        !rig.written_text().contains("threema://"),
        "the link reached a file"
    );
    assert!(!rig.written_text().contains(&digest_shape()));
    rig.assert_ended("a link");
}

/// The refusal of the ID at `linking` ends the login before anything is stored: no question; any
/// other end at `linking` leaves `link_at` for the next start (critic M4 b).
#[test]
fn link_at_stands_for_a_crash_at_linking_and_goes_for_a_refused_id() {
    let rig = Rig::new("s-linkat");
    let mut run = rig.run("link-refused", true, quick());
    run.wait_for("the wait for the ID", |i| i["id"] == json!("waiting"));
    run.call(&json!({"op": "test_id_arrived"}))
        .expect("the gate");
    // The refusal is S1a (T5): the run halts and ends by itself.
    assert_eq!(run.join().expect("ended"), 1, "S1a halts");
    let events = std::fs::read_to_string(rig.dir.events_log()).expect("events");
    let error = events
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).expect("a JSON line"))
        .find(|e| e["event"]["event"] == json!("error"))
        .expect("an error event");
    assert_eq!(
        error["event"]["data"],
        json!({"source": "login", "kind": "NotTheExpectedAccount"}),
        "the kind word only"
    );
    let state = rig.state();
    assert_eq!(state["link_at"], Value::Null);
    assert_eq!(state["halted"]["kind"], json!("link-refused"));
    assert!(!rig.written_text().contains(&id_shape()));
    rig.assert_ended("a refused ID");
    // The other: the app dies while the login stands at linking (S1b, RP-b's review S-1).
    let rig = Rig::new("s-linkat2");
    let mut run = rig.run("link-crash", true, quick());
    run.wait_for("the wait for the ID", |i| i["id"] == json!("waiting"));
    run.call(&json!({"op": "test_id_arrived"}))
        .expect("the gate");
    assert_eq!(
        run.join().expect("ended"),
        1,
        "an exit nobody asked for halts"
    );
    let state = rig.state();
    assert!(
        state["link_at"].is_u64(),
        "the next start asks the question: {state}"
    );
    assert_eq!(state["halted"]["kind"], json!("link-failed-after-hello"));
    let words = state_in(&rig.dir, None).expect("state").join("\n");
    assert!(words.contains("a login stood at linking"), "{words}");
    rig.assert_ended("a crash at linking");
}

// ---- The ends ---------------------------------------------------------------------------------------------

/// END 1, a stop: the app is asked to quit, goes, and only then is the switch written off.
#[test]
fn a_stop_ends_the_app_and_then_writes_the_switch_off() {
    let rig = Rig::new("s-stop");
    let mut run = rig.run("normal", false, quick());
    run.ready();
    assert_eq!(run.info().expect("info")["id"], json!("not-asked"));
    run.stop();
    assert_eq!(run.join().expect("ended"), 0, "a stop is no halt");
    assert!(
        rig.app_saw_the_switch_on(),
        "the switch was still on at the app's last moment"
    );
    rig.assert_ended("a stop");
    assert_eq!(rig.state()["halted"], Value::Null);
    let events = std::fs::read_to_string(rig.dir.events_log()).expect("events");
    assert!(
        events.contains("stop_asked") && events.contains("\"asked\":true"),
        "{events}"
    );
    assert_eq!(rig.seen_commands(), ["quit"]);
}

/// END 2, a crash: the app goes by itself; the run halts `exited`, the switch is written off after.
#[test]
fn a_crash_ends_with_the_switch_off_and_a_halt() {
    let rig = Rig::new("s-crash");
    let run = rig.run("crash", false, quick());
    assert_eq!(run.join().expect("ended"), 1);
    assert!(rig.app_saw_the_switch_on());
    rig.assert_ended("a crash");
    let state = rig.state();
    assert_eq!(state["halted"]["kind"], json!("exited"));
    let events = std::fs::read_to_string(rig.dir.events_log()).expect("events");
    assert!(events.contains("\"asked\":false"), "{events}");
}

/// END 3, the halt of the time limit (critic R-L3): an app with no first answer is killed, the run
/// halts `app-not-up`, the list of test IDs goes, the switch is written off after the app is gone.
#[test]
fn no_first_answer_in_time_halts_kills_and_ends_with_the_switch_off() {
    let rig = Rig::new("s-limit");
    rig.write_list();
    let mut timing = quick();
    timing.first_answer = Duration::from_millis(300);
    let mut run = rig.run("silent", true, timing);
    run.wait_for("the wait for the ID", |i| i["id"] == json!("waiting"));
    run.call(&json!({"op": "test_id_arrived"}))
        .expect("the gate");
    let started = run.wait_for("the app", |i| i["app_pid"].is_u64());
    assert_eq!(started["answered"], json!(false));
    let pid = started["app_pid"].as_u64().expect("a pid");
    assert_eq!(run.join().expect("ended"), 1);
    assert!(!pid_exists(pid), "the silent app was killed and reaped");
    assert!(
        !rig.dir.allow_file().exists(),
        "the list went with the halt"
    );
    assert_eq!(rig.state()["halted"]["kind"], json!("app-not-up"));
    rig.assert_ended("the time limit");
    // The next start is refused without his word (the halt stands in the state).
    let words = state_in(&rig.dir, None).expect("state").join("\n");
    assert!(words.contains("halted:      app-not-up"), "{words}");
}

/// END 4, a signal that would kill the supervisor ends the run in order: the app first, then the
/// switch (a SIGKILL cannot be caught: `stop` writes it off then, below).
#[test]
fn a_signal_ends_the_run_in_order() {
    let rig = Rig::new("s-signal");
    let mut run = rig.run("normal", false, quick());
    run.ready();
    run.terminated.store(true, Ordering::Relaxed);
    assert_eq!(run.join().expect("ended"), 0);
    assert!(rig.app_saw_the_switch_on());
    rig.assert_ended("a signal");
    assert_eq!(rig.state()["halted"], Value::Null);
}

/// The profile lock is held until the app's process is gone AND reaped: whenever it reads free, the
/// app's pid is gone. The app here ignores the quit and is killed after the grace.
#[test]
fn the_lock_is_held_until_the_app_is_reaped() {
    let rig = Rig::new("s-lock");
    let mut timing = quick();
    timing.kill_grace = Duration::from_millis(400);
    let mut run = rig.run("deaf", false, timing);
    let info = run.wait_for("the app", |i| {
        i["app_pid"].is_u64() && i["ready"] == json!(true)
    });
    let pid = info["app_pid"].as_u64().expect("a pid");
    assert_eq!(
        probe_lock(&rig.dir.lock_file(), Duration::ZERO).expect("a probe"),
        Lock::Held,
        "held while the app runs"
    );
    run.stop();
    let mut held_with_a_live_app = 0;
    loop {
        let free = probe_lock(&rig.dir.lock_file(), Duration::ZERO).expect("a probe") == Lock::Free;
        let alive = pid_exists(pid);
        assert!(
            !(free && alive),
            "the lock was free while the app still existed"
        );
        if !free && alive {
            held_with_a_live_app += 1;
        }
        if free {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(
        held_with_a_live_app > 0,
        "the app was seen alive with the lock held"
    );
    assert!(!pid_exists(pid));
    assert_eq!(run.join().expect("ended"), 0);
    rig.assert_ended("a deaf app");
    assert!(!rig.app_saw_the_switch_on(), "a killed app wrote no mark");
}

// ---- The wait for the ID --------------------------------------------------------------------------------

/// A link start waits for the ID before the app starts; the list goes at the app's first answer.
#[test]
fn the_app_starts_after_the_id_and_the_list_goes_at_its_first_answer() {
    let rig = Rig::new("s-id");
    rig.write_list();
    let mut run = rig.run("normal", true, quick());
    let waiting = run.wait_for("the wait for the ID", |i| i["id"] == json!("waiting"));
    assert_eq!(waiting["app_pid"], Value::Null, "no app before the ID");
    assert_eq!(waiting["ready"], json!(false));
    assert!(
        rig.dir.allow_file().exists(),
        "the list stands while the app waits for it"
    );
    run.call(&json!({"op": "test_id_arrived"}))
        .expect("the gate");
    let info = run.ready();
    assert_eq!(info["id"], json!("taken"));
    let limit = Instant::now() + Duration::from_secs(30);
    while rig.dir.allow_file().exists() {
        assert!(
            Instant::now() < limit,
            "the list stands after the first answer"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    run.stop();
    run.join().expect("ended");
    rig.assert_ended("a link start");
}

/// A stop while the ID is awaited: no app is ever started; the switch goes off and the list goes.
#[test]
fn a_stop_while_the_id_is_awaited_starts_no_app() {
    let rig = Rig::new("s-idstop");
    rig.write_list();
    let mut run = rig.run("normal", true, quick());
    run.wait_for("the wait for the ID", |i| i["id"] == json!("waiting"));
    run.stop();
    assert_eq!(run.join().expect("ended"), 0);
    assert!(!rig.dir.allow_file().exists(), "the list went with the end");
    let events = std::fs::read_to_string(rig.dir.events_log()).expect("events");
    assert!(
        events.contains("stopped_before_start") && !events.contains("app_started"),
        "{events}"
    );
    assert!(
        !rig.mark().exists() && !rig.seen().exists(),
        "the app never ran"
    );
    rig.assert_ended("a stop before the app");
}

#[test]
fn an_id_that_never_comes_ends_the_wait() {
    let rig = Rig::new("s-idwait");
    let mut timing = quick();
    timing.id_wait = Duration::from_millis(200);
    let run = rig.run("normal", true, timing);
    assert_eq!(run.join().expect("ended"), 0);
    let events = std::fs::read_to_string(rig.dir.events_log()).expect("events");
    assert!(
        events.contains("id_wait_ended") && !events.contains("app_started"),
        "{events}"
    );
    rig.assert_ended("an id that never came");
}

// ---- The commands the supervisor forwards ---------------------------------------------------------------

#[test]
fn only_the_reading_commands_reach_the_app_and_their_answers_are_redacted() {
    let rig = Rig::new("s-ctl");
    let mut run = rig.run("normal", false, quick());
    run.ready();
    let answer = run
        .call(&json!({"op": "ctl", "cmd": "state", "args": {}, "timeout_ms": 10000}))
        .expect("state");
    assert_eq!(
        answer["result"],
        json!({"banner": "[id]", "digest": "[digest]"}),
        "the app's answer goes through the redaction"
    );
    run.call(&json!({"op": "ctl", "cmd": "chats", "args": {}, "timeout_ms": 10000}))
        .expect("chats");
    let mut sentences = std::collections::BTreeSet::new();
    for cmd in [
        "add_account",
        "send",
        "act",
        "attach",
        "page",
        "quit",
        "fill_login",
        "screenshot",
        "wait",
        "chat_facts",
    ] {
        let e = run
            .call(&json!({"op": "ctl", "cmd": cmd, "args": {}, "timeout_ms": 1000}))
            .expect_err("refused");
        let refused = e.downcast_ref::<Refused>().expect("a refusal");
        assert_eq!(refused.code, "refused", "{cmd}");
        sentences.insert(refused.message.clone());
    }
    // One fixed sentence for all of them: the command's name is never echoed.
    assert_eq!(sentences.len(), 1, "{sentences:?}");
    let bad = run
        .call(&json!({"op": "ctl", "args": {}}))
        .expect_err("no cmd");
    assert_eq!(
        bad.downcast_ref::<Refused>().expect("a refusal").code,
        "badRequest"
    );
    let unknown = run.call(&json!({"op": "nothing"})).expect_err("unknown");
    assert_eq!(
        unknown.downcast_ref::<Refused>().expect("a refusal").code,
        "badRequest"
    );
    // A halted or ending run forwards reading only (`chats` and `state`, never `open_id`).
    run.stop();
    run.join().expect("ended");
    assert_eq!(
        rig.seen_commands(),
        ["state", "chats", "quit"],
        "nothing else reached the app"
    );
    assert!(!rig.written_text().contains(&id_shape()));
}

#[test]
fn a_rehearsals_place_forwards_the_two_cues_of_its_phone_and_nothing_else_new() {
    let rig = Rig::new("s-cue").rehearsing();
    let mut run = rig.run("normal", false, quick());
    run.ready();
    let ctl = |cmd: &str, args: Value| json!({"op": "ctl", "cmd": cmd, "args": args, "timeout_ms": 10000});
    run.call(&ctl("add_account", json!({"network": "Threema"})))
        .expect("the click is forwarded in a rehearsal");
    run.call(&ctl("rehearsal_arrive", json!({"kind": "direct"})))
        .expect("an arrival is forwarded in a rehearsal");
    for (cmd, args) in [
        ("add_account", json!({"network": "Signal"})),
        ("add_account", json!({"network": "Threema", "extra": 1})),
        ("rehearsal_arrive", json!({"kind": "else"})),
        ("rehearsal_act", json!({"kind": "direct", "act": "pin"})),
        ("send", json!({})),
        ("page", json!({"name": "list"})),
    ] {
        let e = run.call(&ctl(cmd, args)).expect_err("refused");
        assert_eq!(
            e.downcast_ref::<Refused>().expect("a refusal").code,
            "refused",
            "{cmd}"
        );
    }
    run.stop();
    run.join().expect("ended");
    assert_eq!(
        rig.seen_commands(),
        ["add_account", "rehearsal_arrive", "quit"],
        "nothing else reached the app"
    );
}

#[test]
fn a_real_profile_never_forwards_the_cues_of_the_rehearsals_phone() {
    let rig = Rig::new("s-cue-real");
    let mut run = rig.run("normal", false, quick());
    run.ready();
    for (cmd, args) in [
        ("add_account", json!({"network": "Threema"})),
        ("rehearsal_arrive", json!({"kind": "direct"})),
    ] {
        let e = run
            .call(&json!({"op": "ctl", "cmd": cmd, "args": args, "timeout_ms": 1000}))
            .expect_err("refused");
        assert_eq!(
            e.downcast_ref::<Refused>().expect("a refusal").code,
            "refused",
            "{cmd}"
        );
    }
    run.stop();
    run.join().expect("ended");
    assert_eq!(
        rig.seen_commands(),
        ["quit"],
        "nothing but the quit reached the app"
    );
}

// ---- state ------------------------------------------------------------------------------------------------

#[test]
fn state_says_what_it_sees_in_words_and_no_value() {
    let rig = Rig::new("s-state");
    // A profile this tool never started.
    let bare = ProfileDir {
        dir: rig.w.keeper().join("bare"),
        rehearsal: false,
    };
    private_dir(&bare.dir).expect("a directory");
    let words = state_in(&bare, None).expect("state").join("\n");
    assert!(words.contains("never started by this tool"), "{words}");
    // No supervisor, the switch ON, a halt, a login at linking, an open question: every word.
    let mut state = State::load(&rig.dir).expect("load").expect("a state");
    state.set(
        "halted",
        json!({"kind": "exited", "at_ms": 1_700_000_000_000_u64}),
    );
    state.set("link_at", json!(1_700_000_000_000_u64));
    state.set("starts", json!(3));
    state.save(&rig.dir).expect("save");
    std::fs::write(
        rig.dir.open_question_file(),
        "{\"kind\": \"link-failed-after-hello\", \"at_ms\": 1}\n",
    )
    .expect("the question");
    let words = state_in(&rig.dir, None).expect("state").join("\n");
    for want in [
        "profile:     p\n",
        "3 start(s); login since never; last online never",
        "halted:      exited at 2023-11-14 22:13:20 UTC (a new start needs --after-halt",
        "a login stood at linking and its end was not seen",
        "question:    open (link-failed-after-hello)",
        "switch:      Threema is ON in the profile",
        "supervisor:  not running",
        "the last run did not end cleanly. Run `threema-run stop`",
    ] {
        assert!(words.contains(want), "{want}\n---\n{words}");
    }
    // An app that still holds its lock with no supervisor.
    let app_lock = open_lock_file_at(&app_lock_file(&rig.dir));
    let words = state_in(&rig.dir, None).expect("state").join("\n");
    assert!(words.contains("still runs without a supervisor"), "{words}");
    drop(app_lock);
    // A running supervisor.
    let mut run = rig.run("normal", false, quick());
    run.ready();
    run.wait_for("four lines", |i| i["lines_total"].as_u64() >= Some(4));
    let words = state_in(&rig.dir, Some(&rig.socket())).expect("state");
    let words = words.join("\n");
    for want in [
        "supervisor:  up (pid ",
        "dev channel up; first answer after ",
        "id:          not-asked",
        "login:       none seen",
        "contract lines: E4_REFUSED 1, ERROR_LINE 1, L_COUNT 1",
    ] {
        assert!(words.contains(want), "{want}\n---\n{words}");
    }
    assert!(!words.contains("warning:"), "{words}");
    // Counts and words only: no path, no value.
    let scratch = rig.w.scratch.0.display().to_string();
    assert!(
        !words.contains(&scratch)
            && !words.contains(&id_shape())
            && !words.contains(&digest_shape())
    );
    run.stop();
    run.join().expect("ended");
}

/// A lock file made and held for as long as the guard lives.
fn open_lock_file_at(path: &Path) -> std::fs::File {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("directory");
    let file = open_lock_file(path).expect("a lock file");
    file.try_lock().expect("the lock");
    file
}

// ---- stop ---------------------------------------------------------------------------------------------------

fn short_stop() -> StopTiming {
    StopTiming {
        supervisor: Duration::from_millis(300),
        app: Duration::from_millis(300),
        poll: Duration::from_millis(10),
    }
}

#[test]
fn stop_writes_the_switch_off_when_a_killed_supervisor_left_it_on() {
    let rig = Rig::new("s-stop-left");
    // Nothing runs, the switch is on (a supervisor that was killed with SIGKILL): written off, said.
    let (words, status) = stop_in(&rig.dir, false, None, &short_stop(), &|_| {}).expect("stop");
    assert_eq!(status, 0);
    let words = words.join("\n");
    assert!(
        words.contains(
            "switched off in the profile (the login stays): the last run did not end cleanly"
        ),
        "{words}"
    );
    assert_eq!(rig.config(), "networks-on = []\n");
    // Again: nothing to do.
    let (words, status) = stop_in(&rig.dir, false, None, &short_stop(), &|_| {}).expect("stop");
    assert_eq!(status, 0);
    assert!(
        words.join("\n").contains("is off in the profile"),
        "{words:?}"
    );
}

#[test]
fn stop_leaves_the_switch_while_the_app_still_runs() {
    let rig = Rig::new("s-stop-live");
    let held = open_lock_file_at(&app_lock_file(&rig.dir));
    let (words, status) = stop_in(&rig.dir, false, None, &short_stop(), &|_| {}).expect("stop");
    assert_eq!(status, 1);
    assert!(
        words
            .join("\n")
            .contains("stays switched ON until it is gone"),
        "{words:?}"
    );
    assert_eq!(
        rig.config(),
        ON,
        "an app that may still run keeps its switch"
    );
    drop(held);
    let (_, status) = stop_in(&rig.dir, false, None, &short_stop(), &|_| {}).expect("stop");
    assert_eq!(status, 0);
    assert_eq!(rig.config(), "networks-on = []\n");
}

#[test]
fn stop_ends_a_live_run() {
    let rig = Rig::new("s-stop-run");
    let mut run = rig.run("normal", false, quick());
    run.ready();
    let timing = StopTiming {
        supervisor: Duration::from_secs(30),
        app: Duration::from_secs(5),
        poll: Duration::from_millis(10),
    };
    let (words, status) = stop_in(&rig.dir, false, Some(&rig.socket()), &timing, &|_| {
        panic!("no signal")
    })
    .expect("stop");
    let words = words.join("\n");
    assert_eq!(status, 0, "{words}");
    assert!(
        words.contains("asked to end the app") && words.contains("supervisor:  ended"),
        "{words}"
    );
    assert_eq!(run.join().expect("ended"), 0);
    rig.assert_ended("stop");
    assert!(rig.app_saw_the_switch_on());
}

/// A supervisor that does not end: `stop` leaves the switch on and says how to go on; `--force` tells
/// it to end (SIGTERM, the pid its own file records) and goes on once it has let go.
#[test]
fn a_supervisor_that_does_not_end_is_told_with_force_and_never_killed_from_here() {
    let rig = Rig::new("s-stop-force");
    let lock = RefCell::new(Some(
        take_profile_lock(&rig.dir).expect("a stuck supervisor's lock"),
    ));
    let (words, status) = stop_in(&rig.dir, false, None, &short_stop(), &|_| {
        panic!("no signal")
    })
    .expect("stop");
    let words = words.join("\n");
    assert_eq!(status, 1);
    assert!(
        words.contains("does not answer")
            && words.contains("stays switched ON")
            && words.contains("--force"),
        "{words}"
    );
    assert_eq!(rig.config(), ON);
    // --force without a recorded pid: nothing to signal.
    let (words, status) = stop_in(&rig.dir, true, None, &short_stop(), &|_| {
        panic!("no signal")
    })
    .expect("stop");
    assert_eq!(status, 1);
    assert!(
        words.join("\n").contains("its pid is not recorded"),
        "{words:?}"
    );
    // With one: the signal goes to that pid, and the stand-in lets go when it gets it.
    std::fs::write(rig.dir.supervisor_file(), "{\"pid\": 4242}").expect("supervisor.json");
    let signalled = RefCell::new(Vec::new());
    let term = |pid: u32| {
        signalled.borrow_mut().push(pid);
        drop(lock.borrow_mut().take());
    };
    let (words, status) = stop_in(&rig.dir, true, None, &short_stop(), &term).expect("stop");
    let words = words.join("\n");
    assert_eq!(status, 0, "{words}");
    assert_eq!(*signalled.borrow(), [4242]);
    assert!(words.contains("--force: told to end (SIGTERM)"), "{words}");
    assert_eq!(rig.config(), "networks-on = []\n");
}

// ---- A supervisor started by hand --------------------------------------------------------------------

#[test]
fn a_supervisor_started_by_hand_starts_nothing_and_touches_nothing() {
    let rig = Rig::new("s-hand");
    let result = supervise_in(
        &rig.dir,
        &Seams {
            uid: current_uid(),
            now: &real_now,
            check: &|_| Ok(stamp()),
            make: &|_| panic!("no app may start"),
            timing: &quick(),
            terminated: &AtomicBool::new(false),
            readable: &|_| false,
            phone: std::sync::Arc::new(|_, _| anyhow::bail!("no phone in this test")),
        },
    );
    assert!(error_of(result).contains("no launch order"));
    assert_eq!(rig.config(), ON, "the switch is the start's own guard's");
    assert_eq!(
        probe_lock(&rig.dir.lock_file(), Duration::ZERO).expect("a probe"),
        Lock::Free
    );
    assert!(!rig.socket().exists() && !rig.dir.supervisor_file().exists());
}
