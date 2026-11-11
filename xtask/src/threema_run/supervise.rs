//! The supervisor (`threema-run __supervise`), its readers, the redaction, `state` and `stop` (T3b;
//! `briefs/tr.md` §2.7, §2.8, §2.12, the critic's R-L3, R-L8, M3 and `wa_run.rs`'s supervisor).
//!
//! `start` ([`super::launch::start`]) spawns this program again, detached, and waits for it to answer
//! `info` on the control socket. The supervisor, in this order:
//!
//! 1. makes itself non-dumpable (the app is, by `KOURIER_DEV_NO_DUMP=1`), takes the PROFILE LOCK and
//!    keeps it until the app's process is gone AND reaped (`start`'s guard and `stop` read a free lock
//!    as "nobody runs"), takes the launch order away (a supervisor started by hand has none and does
//!    nothing), checks the stamped copy, loads the state;
//! 2. binds the order's socket, writes `supervisor.json` (`pid`, `socket`: clients find it there);
//! 3. in a `--for-link` start waits for the ID (`info.id` is `waiting`; T4's `id` operation calls
//!    [`Shared::id_arrived`] after it wrote the list of test IDs; the wait is bounded and a `stop`
//!    ends it), then starts the app DIRECTLY from the stamped copy (the order's environment is
//!    `launch::app_command`), stdin, stdout and stderr piped;
//! 4. reads the app's two outputs (below) and serves the socket until the app is gone;
//! 5. at EVERY end (a stop, a crash, a halt, the time limit, a signal that kills the supervisor) waits
//!    until the app's process is gone and reaped, THEN writes the switch off with
//!    [`launch::switch_off`] (the file as it is then), THEN lets the lock go. A killed supervisor
//!    (SIGKILL) runs no end: the app quits by itself when its stdin ends, the switch stays on, and
//!    `state` and `stop` say so (`stop` writes it off once the app's own lock is free).
//!
//! THE TWO READERS only COLLECT; judging is T5's (the call site is [`Shared::on_app_line`] and
//! [`Shared::on_app_event`], "T5: judge here").
//!
//! * The app's STDERR, line by line: matched RAW in memory against the line table
//!   ([`super::preflight::LINES`], `th6-lines.md`) and the two belts of rule S8 (an ERROR line, a
//!   panic); a match is kept in a ring ([`SeenLine`]); EVERY line goes to `app.log` through
//!   [`redact`] (64 MiB cap). The raw line is never written anywhere and is served only redacted.
//! * The app's STDOUT, the dev control: `channel` (the first answer), `login_step`, `login_qr`
//!   (only `shown`), `error` (only the kind word before `:`), `account_state`; sanitized, redacted,
//!   written to `events.jsonl` and kept in a ring; responses reach whoever asked ([`app_call`]).
//!
//! THE REDACTION ([`redact`]): any run of 64 or more hex digits becomes `[digest]` (a digest of an
//! eight-character ID is as good as the ID) and a standalone token of exactly 8 characters of
//! `[A-Z0-9*]` with a capital letter becomes `[id]`. Every string the tool writes about the app goes
//! through it; the tool never stores the ID.
//!
//! The socket's operations (one JSON line in, one out; `{"ok": true, ...}` or `{"ok": false,
//! "error": {"code", "message"}}`): `info` (the fields `bring_up` reads: `pid`, `ready`, `id`
//! (`waiting|taken|not-asked|gave-up`), `halted`, `self_dumpable`, `app_pid`; and counts and words of
//! the run), `events {since}`, `lines {since}` (the kept contract lines, redacted), `ctl {cmd, args,
//! timeout_ms}` (only `state`, `chats`, `open_id`, `close`; every other command, `add_account`,
//! `send`, `act`, `attach` among them, is refused in every run; ONE exception, a rehearsal's place
//! only: the cues of its fake phone, [`rehearsal_cue_ok`]) and `stop`. `send_window` and
//! `send_window_over` (a send step's bounds for the judge; only the `step` of that name asks).
//! `step_send` (a send step's own send, on the user's word through `--lead-sends`: [`step_send`]). T4's: `allow` (the digest
//! from `threema-run id`) and `answer` (his y or n), both only from a process in the foreground of a
//! terminal; `ask` and `answer_of` (a step's question, [`super::prompt::QUESTIONS`]). T5 adds the
//! halts, T6 and T7 the steps' questions.
//!
//! THE TIME LIMIT (critic R-L3): an app that gives no first answer on its dev control within
//! [`Timing::first_answer`] (60 s from its start) is killed and the run halts `app-not-up`; the
//! start ends (its `bring_up` finds the supervisor gone, or the halt) and the switch goes off. The
//! list of test IDs of a `--for-link` start goes at that first answer and at every end
//! ([`Shared::list_goes`], under the keeper lock the run took when `allow` wrote it; the file's
//! whole life is in [`super::prompt`]).

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::ffi::OsString;
use std::fs::OpenOptions;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::os::unix::fs::OpenOptionsExt as _;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use super::judge::{COUNT_LINES_WITHIN, Judge, Said, Seen, count_lines_missing, judge};
use super::launch::{
    Order, QUESTION_KIND, Stamp, app_command, check_copy, listen_at, open_question_noting,
    socket_for_client, switch_is_on, switch_off, take_launch, take_profile_lock,
};
use super::lines::{Body, Parsed, parse};
use super::preflight::{Lock, NETWORK_NAME, app_lock_file, probe_lock, utc};
use super::profile::{
    ProfileDir, State, append_line, remove_leftover_list, remove_synced, write_private,
};
use super::prompt::{
    LINKING_KIND, NOT_FROM_A_TERMINAL, QUESTIONS, is_digest_hex, linking_ended, mark_linking,
    needs_a_terminal, peer_is_on_a_terminal, peer_pid, question_kind, record_link_answer,
    remove_the_list, wipe, write_the_list,
};
use super::tphone::is_marker_for;
use super::{Args, var};
use crate::target_dir;

// ---- Bounds -----------------------------------------------------------------------------------------

/// How long an app is given for its first answer on the dev control (critic R-L3).
pub(super) const FIRST_ANSWER_WITHIN: Duration = Duration::from_secs(60);
/// How long an app that must go is given to quit by itself before it is killed.
pub(super) const KILL_GRACE: Duration = Duration::from_secs(5);
/// How long a `--for-link` start waits for the ID (well under the 10 minutes one shell call may last).
pub(super) const ID_WAIT: Duration = Duration::from_secs(9 * 60);
/// How long an end lets a link the phone confirmed (the login at `linking`) finish before the app is
/// asked to quit (wa_run.rs:2823 `LINK_END_WAIT`).
pub(super) const LINK_END_WAIT: Duration = Duration::from_secs(120);
/// How long the end of a link that failed after its hello waits for RP-b's drop outcome (D4), from
/// the failed step: the core's 10 s logout and a margin (critic M2).
pub(super) const DROP_WAIT: Duration = Duration::from_secs(12);
/// Events kept for clients that ask later.
const EVENT_RING: usize = 2000;
/// Contract lines kept for clients that ask later.
const LINE_RING: usize = 4096;
/// How much of a kept line is kept (a line of the contract is a hundred bytes or so).
const LINE_MAX: usize = 2048;
/// An `app.log` larger than this at a start becomes `app.log.1`; a run never writes more than this
/// into it.
pub(super) const APP_LOG_CAP: u64 = 64 * 1024 * 1024;
/// The longest request the socket takes.
const REQUEST_MAX: usize = 64 * 1024;
/// How long a lock that looks held is asked for again by `state` and `stop`.
const LOCK_LOOK: Duration = Duration::from_millis(200);

/// What a test shortens (a test asserts outcomes, never a wall time).
pub(super) struct Timing {
    pub first_answer: Duration,
    pub kill_grace: Duration,
    pub id_wait: Duration,
    pub poll: Duration,
    /// How long the lines the app wrote before it died are waited for.
    pub drain: Duration,
    pub log_cap: u64,
    /// How long an end lets a link at `linking` finish.
    pub link_end: Duration,
    /// How long the end of a link that failed after its hello waits for the drop's outcome.
    pub drop_wait: Duration,
    /// MEDIUM-1: how long an online account may go without a count line (a real place only).
    pub count_watch: Duration,
}

impl Timing {
    pub const REAL: Self = Self {
        first_answer: FIRST_ANSWER_WITHIN,
        kill_grace: KILL_GRACE,
        id_wait: ID_WAIT,
        poll: Duration::from_millis(50),
        drain: Duration::from_secs(2),
        log_cap: APP_LOG_CAP,
        link_end: LINK_END_WAIT,
        drop_wait: DROP_WAIT,
        count_watch: COUNT_LINES_WITHIN,
    };
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

fn locked<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn file_len(path: &Path) -> u64 {
    std::fs::metadata(path).map_or(0, |m| m.len())
}

// ---- The redaction ----------------------------------------------------------------------------------

/// Replaces every maximal run of bytes that `in_run` accepts (ASCII only, so every cut falls on a
/// character boundary) for which `replace` names a replacement.
fn replace_runs(
    text: &str,
    in_run: impl Fn(u8) -> bool,
    replace: impl Fn(&str) -> Option<&'static str>,
) -> String {
    let b = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let (mut i, mut copied) = (0, 0);
    while i < b.len() {
        if !in_run(b[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < b.len() && in_run(b[i]) {
            i += 1;
        }
        if let Some(with) = replace(&text[start..i]) {
            out.push_str(&text[copied..start]);
            out.push_str(with);
            copied = i;
        }
    }
    out.push_str(&text[copied..]);
    out
}

/// The tool's own fixed upper-case words that have the shape of an ID (see [`is_an_id_token`]).
const OWN_WORDS: [&str; 4] = ["PERSONAL", "QUESTION", "MENTIONS", "TERMINAL"];

/// A token of exactly eight characters of `[A-Z0-9*]` with at least one capital letter: the shape of
/// a Threema ID (a Gateway ID starts with `*`). English words in the log are lower case and Rust's
/// kinds are mixed case, so neither has it; a digit run (a counter) has no capital. The tool's OWN
/// upper-case words of that length ([`OWN_WORDS`]: `PERSONAL` names the user's phone in every note) are
/// no ID: RT1 (2026-11-05) printed "[id] phone" in the report. The list is fixed and short; a value
/// from outside that merely has the shape still becomes `[id]`.
pub(super) fn is_an_id_token(token: &str) -> bool {
    !OWN_WORDS.contains(&token)
        && token.len() == 8
        && token
            .bytes()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == b'*')
        && token.bytes().any(|c| c.is_ascii_uppercase())
}

/// Takes out of a line what must reach no file of the tool (critic R-L8): a run of 64 or more hex
/// digits (a digest of the ID is treated as the ID: 36^8 tries) becomes `[digest]` and the shape of
/// an ID becomes `[id]`. The app's own rules keep both out of its log already; this is the second
/// layer.
pub(super) fn redact(text: &str) -> String {
    let no_digests = replace_runs(
        text,
        |c| c.is_ascii_hexdigit(),
        |run| (run.len() >= 64).then_some("[digest]"),
    );
    replace_runs(
        &no_digests,
        |c| c.is_ascii_alphanumeric() || c == b'*',
        |token| is_an_id_token(token).then_some("[id]"),
    )
}

/// A JSON value with [`redact`] applied to every string in it (the keys stay: they are the
/// protocol's words).
pub(super) fn redact_value(v: &Value) -> Value {
    match v {
        Value::String(text) => Value::String(redact(text)),
        Value::Array(items) => Value::Array(items.iter().map(redact_value).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, value)| (key.clone(), redact_value(value)))
                .collect(),
        ),
        other => other.clone(),
    }
}

// ---- What the readers collect -----------------------------------------------------------------------

/// One collected line of the app's log, raw (in the supervisor's memory only: served redacted).
#[derive(Clone, Debug)]
struct SeenLine {
    seq: u64,
    at_ms: u64,
    row: &'static str,
    line: String,
}

/// `s` cut to at most `max` bytes on a character boundary.
fn cut(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// `app.log`: every stderr line of the app, redacted, capped (a run at this log level writes a lot).
pub(super) struct LogSink {
    file: Option<std::fs::File>,
    written: u64,
    cap: u64,
    capped: bool,
}

impl LogSink {
    /// Opens `path` for appending (0600). A log of earlier runs larger than `cap` is kept once as
    /// `app.log.1`.
    pub fn open(path: &Path, cap: u64) -> Self {
        if file_len(path) > cap {
            let mut old = path.as_os_str().to_owned();
            old.push(".1");
            let _ = std::fs::rename(path, PathBuf::from(old));
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(path)
            .ok();
        Self {
            file,
            written: file_len(path),
            cap,
            capped: false,
        }
    }

    /// Writes one line that is already redacted. Past the cap one marker line says so and nothing
    /// more is kept.
    pub fn line(&mut self, text: &str) {
        let Some(file) = self.file.as_mut() else {
            return;
        };
        if self.written >= self.cap {
            if !self.capped {
                self.capped = true;
                let _ = writeln!(
                    file,
                    "==== threema-run: app.log reached its cap: later lines are not kept ===="
                );
            }
            return;
        }
        if writeln!(file, "{text}").is_ok() {
            self.written += u64::try_from(text.len() + 1).unwrap_or(u64::MAX);
        }
    }
}

/// Where the supervisor is with the ID of a `--for-link` start (it never holds the ID: T4's `id`
/// operation writes the list and says it arrived).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IdState {
    /// A start that is not for a link.
    NotAsked,
    Waiting,
    Taken,
    /// The wait ran out, or the run was stopped first.
    GaveUp,
}

impl IdState {
    fn word(self) -> &'static str {
        match self {
            Self::NotAsked => "not-asked",
            Self::Waiting => super::launch::INFO_ID_WAITING,
            Self::Taken => "taken",
            Self::GaveUp => "gave-up",
        }
    }
}

/// The commands of the dev channel a run forwards for a client at all (tr.md §2.8: `state`, `chats`,
/// `open_id`, `close`; no `wait`, no `chat_facts`; `add_account`, `send`, `act` and `attach` are
/// refused in every run, a page change is the supervisor's own end).
const CTL_FORWARDED: [&str; 4] = ["state", "chats", "open_id", "close"];
/// What a halted or ending run still forwards: reading, never acting.
const CTL_WHEN_HALTED: [&str; 2] = ["state", "chats"];

/// The one text a rehearsal's walk may send through its cue (in the real place a send is the user's
/// own act in the window or a `--lead-sends` step's own, [`step_send`]; `ctl` never forwards one).
pub(super) const REHEARSAL_SEND_TEXT: &str = "rehearsal send";

/// The send steps: the words a step gives the supervisor when it opens or closes its window.
pub(super) const SEND_WINDOWS: [&str; 3] = ["send", "group-send", "send-file"];

/// The bounds of a step's own send ([`step_send`]): the chat opening or a file reaching the composer,
/// the core looking at the file, the app's answer (it waits up to 15 s for the row to show).
const STEP_SEND_CHAT_WITHIN: Duration = Duration::from_secs(20);
const STEP_SEND_FILE_WITHIN: Duration = Duration::from_secs(60);
const STEP_SEND_ANSWER_WITHIN: Duration = Duration::from_secs(40);
const STEP_SEND_POLL: Duration = Duration::from_millis(100);
/// The client's bound on the whole of [`step_send`], above the sum of its stages.
pub(super) const STEP_SEND_WITHIN: Duration = Duration::from_secs(180);

/// The tool's picture, in the tool's own directory of the profile: its name and side.
const STEP_PICTURE: &str = "step-send-picture.png";
const STEP_PICTURE_SIDE: u32 = 128;

/// The cues the rehearsal's walk (`rehearse.rs`) gives its fake phone, forwarded in a rehearsal's
/// place ONLY (a real profile never has one: its start needs the user's go and the real network):
/// `add_account` for Threema (the click of "Add Account" that no person makes in a rehearsal),
/// `rehearsal_arrive` for a direct or a group chat (the PERSONAL phone's text) and `send` of the one
/// rehearsal text (the user's own send in the window). Nothing else rides on them: no other key in
/// the arguments (wa_run.rs:3437 `rehearsal_arrive`, the same rule).
pub(super) fn rehearsal_cue_ok(cmd: &str, args: &Value) -> bool {
    let one_key = args.as_object().is_some_and(|o| o.len() == 1);
    match cmd {
        "add_account" => one_key && args["network"] == json!(NETWORK_NAME),
        "rehearsal_arrive" => one_key && matches!(args["kind"].as_str(), Some("direct" | "group")),
        "send" => {
            args.as_object().is_some_and(|o| o.len() == 2)
                && args["text"] == json!(REHEARSAL_SEND_TEXT)
                && args["conversation"].as_i64().is_some_and(|id| id > 0)
        }
        _ => false,
    }
}
/// What `stop` and the end ask the app: the one command of the dev channel that quits it.
const QUIT: &str = "quit";

/// Opens the TEST phone of a run by its serial (a rehearsal's is always the scripted fake).
pub(super) type PhoneOpener =
    Arc<dyn Fn(&ProfileDir, &str) -> Result<Box<dyn crate::test_phone::Phone>> + Send + Sync>;

/// Everything the supervisor's threads share.
struct Shared {
    dir: ProfileDir,
    for_link: bool,
    /// The state file as the supervisor keeps it.
    state: State,
    dirty: bool,
    log: LogSink,
    /// The app's dev channel: one JSON command per line.
    stdin: Option<ChildStdin>,
    next_id: u64,
    /// Commands sent to the app that wait for their response.
    pending: HashMap<u64, mpsc::Sender<Value>>,
    events: VecDeque<(u64, Value)>,
    next_seq: u64,
    lines: VecDeque<SeenLine>,
    next_line: u64,
    /// How many lines each row of the table (or belt) was seen, since the app started.
    row_counts: BTreeMap<&'static str, u64>,
    lines_total: u64,
    /// HEAD's migration files (the stamp's): a file is written into the state's record only once
    /// the app opened the database it belongs to.
    migrations: BTreeMap<String, String>,
    core_schema: Option<u64>,
    id: IdState,
    app_pid: Option<u32>,
    started_ms: u64,
    /// The first JSON object the app wrote on its dev channel (the `channel` event): `ms` after the
    /// start.
    first_answer_ms: Option<u64>,
    /// The `channel` event was seen: the dev channel is up.
    ready: bool,
    /// The last login step of the app and its kind.
    login: Option<(String, String)>,
    login_seen: bool,
    account: Option<(String, String, Value)>,
    online: bool,
    /// `stop` came (or a signal): not a halt.
    stop_asked: bool,
    /// The run is ending (a stop or a halt): the ending thread asks the app to quit.
    ending: bool,
    ending_started: bool,
    /// The app must go: asked to quit, killed after the grace by the loop.
    kill_requested: bool,
    /// The app must go now (it never answered): killed without the grace.
    kill_now: bool,
    /// How the app's process ended (`status`), once it has.
    exit: Option<String>,
    /// The KEEPER LOCK, held from the moment `allow` wrote the list of test IDs until the list is
    /// gone ([`Shared::list_goes`]): two link starts of two profiles never overlap (T4).
    keeper_lock: Option<std::fs::File>,
    /// Where the list of test IDs of this run is: one of the `LIST_*` words.
    list: &'static str,
    /// The question a step put to the user ([`QUESTIONS`], by name); `None` when none is open.
    question: Option<String>,
    /// The latest answer: the question's name and his y (`true`) or n.
    answer: Option<(String, bool)>,
    /// Where the latest answer came from: `typed` (his terminal) or `relayed` (the lead, after his
    /// word in chat; only in a run with a test phone).
    answer_origin: &'static str,
    /// The digest of the TEST ID, in memory only, while the run lives: the TEST phone's `my_id`
    /// is compared with it (`same_id`). Never written, printed or logged.
    allowed_digest: Option<String>,
    /// How the supervisor opens the TEST phone (B1): by the serial it was started with, never from a
    /// client's say-so. A test hands in a scripted one.
    phones: PhoneOpener,
    /// THE JUDGE's context (T5): what the lines and events so far left.
    judge: Judge,
    /// LOW-7: the secret files of the app's `/proc` entry that this user could read right after its
    /// start (words); empty when it is protected, as the stamped copy's mode makes it.
    app_exposed: Vec<&'static str>,
    /// The bounds of the end (a test shortens them).
    link_end: Duration,
    drop_wait: Duration,
}

/// The words of `info.allow_list`: no list in this run, the list stands (the app has not answered
/// yet), the list went at the app's first answer (path 1 of the file's life: the app took it), or at
/// an end before that (path 2).
const LIST_NONE: &str = "none";
const LIST_STANDS: &str = "standing";
pub(super) const LIST_TAKEN: &str = "taken by the start, removed at its first answer";
pub(super) const LIST_AT_END: &str = "removed at the end";

impl Shared {
    fn new(dir: &ProfileDir, order: &Order, state: State, stamp: &Stamp, timing: &Timing) -> Self {
        let core_schema = core_schema_of(&stamp.migrations);
        Self {
            dir: dir.clone(),
            for_link: order.for_link,
            state,
            dirty: false,
            log: LogSink::open(&dir.app_log(), timing.log_cap),
            stdin: None,
            next_id: 0,
            pending: HashMap::new(),
            events: VecDeque::new(),
            next_seq: 0,
            lines: VecDeque::new(),
            next_line: 0,
            row_counts: BTreeMap::new(),
            lines_total: 0,
            migrations: stamp.migrations.clone(),
            core_schema,
            id: if order.for_link {
                IdState::Waiting
            } else {
                IdState::NotAsked
            },
            app_pid: None,
            started_ms: 0,
            first_answer_ms: None,
            ready: false,
            login: None,
            login_seen: false,
            account: None,
            online: false,
            stop_asked: false,
            ending: false,
            ending_started: false,
            kill_requested: false,
            kill_now: false,
            exit: None,
            keeper_lock: None,
            list: LIST_NONE,
            question: None,
            answer: None,
            answer_origin: "typed",
            allowed_digest: None,
            phones: Arc::new(super::tphone::open_phone),
            judge: Judge::default(),
            app_exposed: Vec::new(),
            link_end: timing.link_end,
            drop_wait: timing.drop_wait,
        }
    }

    /// The kind word of the run's halt, if it has one.
    fn halted(&self) -> Option<String> {
        self.state.halt_kind().map(str::to_owned)
    }

    /// T4's `id` operation calls this once the list of test IDs is written: the wait is over.
    pub(super) fn id_arrived(&mut self) {
        if self.id == IdState::Waiting {
            self.id = IdState::Taken;
            self.push_event(json!({"type": "event", "event": "id_taken", "data": {}}));
        }
    }

    fn push_event(&mut self, event: Value) {
        // Every record the tool writes goes through the redaction.
        let event = redact_value(&event);
        let line = json!({"t": now_ms(), "seq": self.next_seq, "event": event});
        let _ = append_line(&self.dir.events_log(), &line.to_string());
        if self.events.len() >= EVENT_RING {
            self.events.pop_front();
        }
        self.events.push_back((self.next_seq, event));
        self.next_seq += 1;
    }

    fn save_state(&mut self) {
        if let Err(e) = self.state.save(&self.dir) {
            eprintln!("threema-run supervisor: cannot write the state file: {e:#}");
        }
        self.dirty = false;
    }

    /// Records the migration files `wanted` names as RAN (a name already recorded keeps its blob id).
    fn record_ran(&mut self, wanted: &dyn Fn(&str) -> bool) {
        let mut ran = self.state.migrations();
        let before = ran.len();
        for (name, blob) in &self.migrations {
            if wanted(name) {
                ran.entry(name.clone()).or_insert_with(|| blob.clone());
            }
        }
        if ran.len() != before {
            self.state.set("migrations", json!(ran));
            self.dirty = true;
        }
    }

    /// core.db is open (the dev channel is up): the core's migrations ran.
    fn core_ran(&mut self) {
        self.record_ran(&|n| n.starts_with("core/"));
    }

    /// The account's `backend.db` is open: the outbox's and Threema's migrations ran in it.
    fn backend_ran(&mut self) {
        self.record_ran(&|n| n.starts_with("outbox/") || n.starts_with("threema/"));
    }

    /// The run stops here: written down at once, kept until `start --after-halt`. The first cause
    /// stays (a later one is an event, not a rewrite of why the run stopped). Every halt ends the
    /// run (tr.md §2.10); the ending thread does the rest ([`end_the_run`]).
    fn halt(&mut self, kind: &str) {
        if self.halted().is_none() {
            self.state
                .set("halted", json!({"kind": kind, "at_ms": now_ms()}));
        }
        self.push_event(json!({"type": "event", "event": "halted", "data": {"kind": kind}}));
        self.save_state();
        // THE HALT'S END: the ending thread stops the app (a link let finish, a waiting login
        // closed, the drop's outcome waited for, then `quit`).
        self.ending = true;
    }

    /// THE JUDGE over one thing seen, in order (T5): a halt it names is raised at once.
    fn judged(&mut self, seen: &Seen<'_>) {
        self.judge.ending = self.ending;
        if let Some(kind) = judge(&mut self.judge, seen, now_ms()) {
            self.halt(&kind);
        }
    }

    /// The drop's outcome goes into the halt's record (`halted.drop`) and an event: `removed`,
    /// `not-reached`, or `not-confirmed` when no outcome line came in time.
    fn note_drop(&mut self) {
        let word = self.judge.drop.unwrap_or("not-confirmed");
        let mut halted = self.state.get("halted").clone();
        if halted.is_object() {
            halted["drop"] = json!(word);
            self.state.set("halted", halted);
        }
        self.push_event(json!({"type": "event", "event": "drop_outcome", "data": {"drop": word}}));
        self.save_state();
    }

    /// One raw stderr line of the app: collected when it is a line the tool reads and written to
    /// `app.log` redacted, never raw; parsed into counts and names and JUDGED, in order.
    fn on_app_line(&mut self, raw: &str) {
        self.lines_total += 1;
        self.log.line(&redact(raw));
        if let Some(parsed) = parse(raw) {
            let row = parsed.row;
            *self.row_counts.entry(row).or_insert(0) += 1;
            if self.lines.len() >= LINE_RING {
                self.lines.pop_front();
            }
            self.lines.push_back(SeenLine {
                seq: self.next_line,
                at_ms: now_ms(),
                row,
                line: cut(raw, LINE_MAX).to_owned(),
            });
            self.next_line += 1;
            if matches!(parsed.body, Body::Unreadable) {
                self.push_event(
                    json!({"type": "event", "event": "line_unreadable", "data": {"row": row}}),
                );
            }
            self.judge_line(&parsed);
        }
    }

    fn judge_line(&mut self, parsed: &Parsed) {
        self.judged(&Seen::Line(parsed));
    }

    /// The first JSON object the app wrote: its dev control answers. The list of test IDs of a
    /// link start has done its work (the app took it or never will): it goes now.
    fn on_first_answer(&mut self) {
        if self.first_answer_ms.is_some() {
            return;
        }
        self.first_answer_ms = Some(now_ms().saturating_sub(self.started_ms));
        if self.for_link {
            self.list_goes(LIST_TAKEN);
        }
    }

    /// The list of test IDs goes: removed (unlink + an fsync of the directory), THEN the keeper lock
    /// this run held is let go ([`remove_the_list`]); recorded once with the path that took it.
    fn list_goes(&mut self, path: &'static str) {
        let held = self.keeper_lock.take();
        match remove_the_list(&self.dir, held) {
            Ok(true) => {
                if self.list != LIST_TAKEN {
                    self.list = path;
                }
                self.push_event(
                    json!({"type": "event", "event": "allow_list", "data": {"removed": path}}),
                );
            }
            Ok(false) => {
                if self.list == LIST_STANDS {
                    self.list = path;
                }
            }
            Err(e) => {
                eprintln!(
                    "threema-run supervisor: the list of test IDs could not be removed ({e:#}): remove it by hand, it stands in the keeper root"
                );
                // LOW-6, fail closed: a list that stands with no lock is readable by the next start of
                // any profile of the keeper root. The run ends; `info.allow_list` keeps saying it
                // stands.
                self.halt("list-not-removed");
            }
        }
    }

    /// One event of the app's dev channel: sanitized, what the tool knows updated, kept, then JUDGED
    /// in its sanitized words (in order).
    fn on_app_event(&mut self, mut v: Value) {
        self.on_first_answer();
        let name = v["event"].as_str().unwrap_or_default().to_owned();
        let now = now_ms();
        match name.as_str() {
            "channel" => {
                self.ready = true;
                self.core_ran();
                // A linked profile's account actor opens its `backend.db` as the app starts.
                if self.state.linked() {
                    self.backend_ran();
                }
            }
            "login_qr" => {
                // The code the phone scans carries the pairing keys: never stored, never passed on,
                // whatever the app sent. Only that it was shown.
                v["data"] = json!({"shown": true});
            }
            "login_step" => {
                self.login_seen = true;
                let step = word_of(&v["data"]["step"]);
                let kind = word_of(&v["data"]["kind"]);
                // A pending account has its `backend.db` from the first step on.
                self.backend_ran();
                if step == "linking" && !self.state.link_stood() {
                    // From here on a device MAY be registered on the TEST phone (critic M4 b): written
                    // and fsynced BEFORE the event is passed on, so a supervisor that dies at this
                    // step is found by the next start, which opens the question. The keeper root
                    // keeps it too (M4 a, T4): every link start of every profile refuses while it
                    // stands, whatever happens to this profile.
                    if let Err(e) = mark_linking(&self.dir, now) {
                        eprintln!(
                            "threema-run supervisor: the keeper root could not keep `linking` ({e:#})"
                        );
                    }
                    self.state.set("link_at", json!(now));
                    self.save_state();
                }
                if kind == "done" {
                    let _ = linking_ended(&self.dir);
                    self.state.set("link_at", Value::Null);
                    self.state.set("linked_since_ms", json!(now));
                    self.state
                        .set("core_schema_at_link", json!(self.core_schema));
                    self.dirty = true;
                }
                v["data"] = json!({"step": step, "kind": kind});
                self.login = Some((step, kind));
            }
            "account_state" => {
                let account = word_of(&v["data"]["state"]);
                let conn = word_of(&v["data"]["conn"]);
                let device = v["data"]["device"].clone();
                self.online = account == "active" && conn == "online";
                if self.online {
                    self.backend_ran();
                    self.state.set("last_online_ms", json!(now));
                    self.dirty = true;
                }
                v["data"] = json!({"state": account, "conn": conn, "device": device});
                self.account = Some((account, conn, device));
            }
            "error" => {
                let source = word_of(&v["data"]["source"]);
                let kind = kind_word(v["data"]["message"].as_str().unwrap_or_default());
                // A refusal of the ID at `linking` ended the login before anything was stored or
                // registered (S1a): nothing may be registered, so no question stands.
                let at_linking = self
                    .login
                    .as_ref()
                    .is_some_and(|(step, _)| step == "linking");
                if source == "login" && at_linking && kind == "NotTheExpectedAccount" {
                    let _ = linking_ended(&self.dir);
                    self.state.set("link_at", Value::Null);
                    self.dirty = true;
                }
                let mut data = json!({"source": source, "kind": kind});
                if let Some(action) = v["data"]["action"].as_str() {
                    data["action"] = json!(word(action));
                }
                v["data"] = data;
            }
            _ => v["data"] = redact_value(&v["data"]),
        }
        let data = v["data"].clone();
        self.push_event(v);
        let said = |key: &str| data[key].as_str().unwrap_or_default().to_owned();
        match name.as_str() {
            "login_step" => {
                let (step, kind) = (said("step"), said("kind"));
                self.judged(&Seen::LoginStep {
                    step: &step,
                    kind: &kind,
                });
            }
            "error" if said("source") == "login" => {
                let kind = said("kind");
                self.judged(&Seen::LoginError { kind: &kind });
            }
            "account_state" => {
                let (state, conn) = (said("state"), said("conn"));
                self.judged(&Seen::Account {
                    state: &state,
                    conn: &conn,
                });
            }
            _ => {}
        }
    }

    /// Asks the app to quit and closes its dev channel (closing stdin quits it too, should the request
    /// be lost).
    fn quit(&mut self) {
        self.next_id += 1;
        let id = self.next_id;
        if let Some(stdin) = self.stdin.as_mut() {
            let _ = writeln!(stdin, "{}", json!({"id": id, "cmd": QUIT}));
            let _ = stdin.flush();
        }
        self.stdin = None;
        self.push_event(json!({"type": "event", "event": "stop_requested", "data": {}}));
    }
}

/// The highest `core/<number>_...` migration of a stamp (what the core's schema version will be).
fn core_schema_of(migrations: &BTreeMap<String, String>) -> Option<u64> {
    migrations
        .keys()
        .filter_map(|name| name.strip_prefix("core/"))
        .filter_map(|name| name.split('_').next()?.parse::<u64>().ok())
        .max()
}

/// A word of the protocol (letters, digits, `_`, `-`, at most 40 of them), or `other`: nothing free
/// goes into a record through a field that is meant to be a word.
pub(super) fn word(text: &str) -> String {
    let ok = !text.is_empty()
        && text.len() <= 40
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if ok { redact(text) } else { "other".to_owned() }
}

fn word_of(v: &Value) -> String {
    word(v.as_str().unwrap_or_default())
}

/// The kind word of an error's message, `"{kind}: {detail}"` (`kourier-model/src/error.rs`): the part
/// before the first `:` only, never the detail.
pub(super) fn kind_word(message: &str) -> String {
    let kind = message.split(':').next().unwrap_or_default().trim();
    word(&kind.replace(' ', "-"))
}

// ---- The socket's operations ------------------------------------------------------------------------

fn refuse(code: &str, message: &str) -> Value {
    json!({"ok": false, "error": {"code": code, "message": message}})
}

fn info_of(s: &Shared) -> Value {
    json!({
        "ok": true,
        "pid": std::process::id(),
        // Whether this process could be dumped to a core file: it must never be true.
        "self_dumpable": is_dumpable(),
        "rehearsal": s.dir.rehearsal,
        "ready": s.ready,
        "answered": s.first_answer_ms.is_some(),
        "first_answer_ms": s.first_answer_ms,
        "id": s.id.word(),
        "halted": s.halted(),
        "app_pid": s.app_pid,
        "started_ms": s.started_ms,
        "stop_asked": s.stop_asked,
        "ending": s.ending,
        "exit": s.exit,
        "login_seen": s.login_seen,
        "login": s.login.as_ref().map(|(step, kind)| json!({"step": step, "kind": kind})),
        "link_at": s.state.link_stood(),
        "account": s.account.as_ref().map(|(state, conn, device)| json!({"state": state, "conn": conn, "device": device})),
        "online": s.online,
        "next_seq": s.next_seq,
        "next_line": s.next_line,
        "lines": s.row_counts,
        "lines_total": s.lines_total,
        "allow_list": s.list,
        // LOW-7: whether the app's environment or memory map was readable by this user's other
        // processes right after its start (a rehearsal never asks: its namespace owner reads all).
        "app_readable": s.app_pid.map(|_| !s.app_exposed.is_empty()),
        "question": s.question,
        // D1: the run has a test phone (its steps check and act on it).
        "test_phone": s.state.test_phone().is_some(),
        // N3: the serial the steps take (the supervisor's own copy, the one `start` printed).
        "test_phone_serial": s.state.test_phone(),
        // The judge's view (T5): the live socket leads (D1 seen on it; T7's `flight-mode` reads it
        // first, critic M5), and RP-b's drop outcome when one came.
        "leader": s.judge.leader,
        // T-H1's step-asides the run saw (the judge reads them as no halt; the report names them).
        "step_asides": s.judge.asides,
        "drop": s.judge.drop,
        // T7: a removal was asked (`unlink`, `removed-on-phone`): 4113, `logged_out` and the drop
        // frame are the removal's own then (R-L2).
        "removal": s.judge.removal_asked,
        "send_window": s.judge.send.is_open(),
    })
}

/// One request of the socket (wa_run.rs:4225 `handle`).
fn handle(shared: &Arc<Mutex<Shared>>, request: &Value) -> Value {
    match request["op"].as_str() {
        Some("info") => info_of(&locked(shared)),
        Some("events") => {
            let since = request["since"].as_u64().unwrap_or(0);
            let s = locked(shared);
            let events: Vec<Value> = s
                .events
                .iter()
                .filter(|(seq, _)| *seq >= since)
                .map(|(seq, e)| json!({"seq": seq, "event": e["event"], "data": e["data"]}))
                .collect();
            json!({"ok": true, "events": events, "next_seq": s.next_seq})
        }
        Some("lines") => {
            let since = request["since"].as_u64().unwrap_or(0);
            let s = locked(shared);
            let lines: Vec<Value> = s
                .lines
                .iter()
                .filter(|l| l.seq >= since)
                .map(|l| {
                    json!({"seq": l.seq, "at_ms": l.at_ms, "row": l.row, "line": redact(&l.line)})
                })
                .collect();
            json!({"ok": true, "lines": lines, "next_seq": s.next_line})
        }
        Some("ctl") => ctl(shared, request),
        Some("stop") => {
            let mut s = locked(shared);
            if !s.stop_asked {
                s.stop_asked = true;
                s.ending = true;
                s.push_event(json!({"type": "event", "event": "stop_asked", "data": {}}));
            }
            json!({"ok": true, "stopping": true})
        }
        Some("allow") => allow(shared, request),
        // B1: the TEST ID read from the TEST phone by `id --from-test-phone`. The SUPERVISOR reads
        // the phone itself, at the serial it was started with, and hashes in its own process; the
        // request carries NO digest (one that does is refused), so no client can choose the list.
        Some("allow_phone") => allow_from_phone(shared, request),
        // `test-phone-in-hand`: the supervisor reads the phone's My ID itself and compares its digest
        // with the one it holds, in memory. Nothing but a yes or no crosses the socket.
        Some("hand") => hand(shared),
        Some("ask") => {
            let Some(name) = request["name"]
                .as_str()
                .filter(|n| *n != QUESTION_KIND && QUESTIONS.iter().any(|(q, _)| q == n))
            else {
                return refuse("badRequest", "that is not a question a step may ask");
            };
            let mut s = locked(shared);
            s.question = Some(name.to_owned());
            s.answer = None;
            s.push_event(
                json!({"type": "event", "event": "question_asked", "data": {"name": name}}),
            );
            json!({"ok": true})
        }
        Some("answer") => answer(shared, request),
        // D5: the lead relays his y or n after his word in chat; only a run with a test phone.
        Some("relay_answer") => {
            // H5/M7: the three emoji are a security comparison on his own phone: typed in his terminal.
            if request["name"].as_str() == Some("emoji-equal") {
                return refuse(
                    "typedOnly",
                    "the emoji comparison is answered by him in his own terminal (`threema-run answer`), never relayed",
                );
            }
            if locked(shared).state.test_phone().is_none() {
                return refuse(
                    "noTestPhone",
                    "this run has no test phone: his answer is typed in `threema-run answer` in his terminal. Nothing was taken",
                );
            }
            answer_with(shared, request, "relayed")
        }
        // T6: what a step heard from him or saw (S7, S9, S12): the judge decides whether it halts.
        Some("said") => {
            let Some(said) = request["what"].as_str().and_then(Said::from_word) else {
                return refuse("badRequest", "that is not a thing a step may say");
            };
            let mut s = locked(shared);
            s.push_event(json!({"type": "event", "event": "said", "data": {"what": said.word()}}));
            s.judged(&Seen::Said(said));
            json!({"ok": true, "halted": s.halted()})
        }
        // T7: a removal he asked for at the end of the run (`unlink`, `removed-on-phone`): the judge
        // lets 4113, `logged_out` and the drop frame pass from now on (R-L2). A step sets it BEFORE
        // it tells him to act. A halted run takes none (it is ending).
        Some("removal") => {
            let what = request["what"].as_str().unwrap_or_default();
            if !matches!(what, "unlink" | "removed-on-phone") {
                return refuse("badRequest", "that is not a removal a step may ask for");
            }
            let mut s = locked(shared);
            if s.halted().is_some() {
                return refuse("halted", "the run is halted: it asks for no removal");
            }
            s.judge.removal_asked = true;
            s.push_event(
                json!({"type": "event", "event": "removal_asked", "data": {"what": what}}),
            );
            json!({"ok": true})
        }
        // LOW-9: an end step that failed gives the removal back: a 4113, a `logged_out` and a drop
        // frame are halts again for the rest of the start.
        Some("removal_over") => {
            let what = request["what"].as_str().unwrap_or_default();
            if !matches!(what, "unlink" | "removed-on-phone") {
                return refuse("badRequest", "that is not a removal a step may ask for");
            }
            let mut s = locked(shared);
            if s.judge.removal_asked {
                s.judge.removal_asked = false;
                s.push_event(
                    json!({"type": "event", "event": "removal_over", "data": {"what": what}}),
                );
            }
            json!({"ok": true})
        }
        // A send step's window: frames of origin `transmit` within the judge's bounds are the user's
        // own send while it stands (the step opens it BEFORE it tells them to send). A halted run
        // takes none.
        Some("send_window") => {
            let what = request["what"].as_str().unwrap_or_default();
            if !SEND_WINDOWS.contains(&what) {
                return refuse("badRequest", "that is not a send step");
            }
            let mut s = locked(shared);
            if s.halted().is_some() {
                return refuse("halted", "the run is halted: it opens no send window");
            }
            s.judge.send.open();
            s.push_event(json!({"type": "event", "event": "send_window", "data": {"what": what}}));
            json!({"ok": true})
        }
        // The step is over: what it wrote stays allowed, nothing further is.
        Some("send_window_over") => {
            let what = request["what"].as_str().unwrap_or_default();
            if !SEND_WINDOWS.contains(&what) {
                return refuse("badRequest", "that is not a send step");
            }
            let mut s = locked(shared);
            if s.judge.send.is_open() {
                s.judge.send.close();
                s.push_event(
                    json!({"type": "event", "event": "send_window_over", "data": {"what": what}}),
                );
            }
            json!({"ok": true})
        }
        Some("step_send") => step_send(shared, request),
        // T6: the one page change a step may ask for: leave a login that waits at its code or its
        // emoji (his "no" to "is it the TEST phone": nobody may scan). Not a halt.
        Some("close_login") => close_login(shared),
        Some("answer_of") => {
            let name = request["name"].as_str().unwrap_or_default();
            let s = locked(shared);
            let answer = s
                .answer
                .as_ref()
                .filter(|(answered, _)| answered == name)
                .map(|(_, yes)| *yes);
            let origin = s.answer_origin;
            json!({"ok": true, "answer": answer, "origin": origin})
        }
        // The door a test uses to move the gate without `id` (T4's `allow` writes the list, then
        // calls `id_arrived`): it only moves the gate and exists in no other build.
        #[cfg(test)]
        Some("test_id_arrived") => {
            locked(shared).id_arrived();
            json!({"ok": true})
        }
        // The door a test answers a step's question through: his y or n without the terminal rule
        // (the same function; `answer` itself is refused to a process that is not on a terminal).
        #[cfg(test)]
        Some("test_answer") => answer(shared, request),
        // The digest of the TEST ID, as `allow` leaves it in memory, without the list's whole life.
        #[cfg(test)]
        Some("test_digest") => {
            locked(shared).allowed_digest = request["digest"].as_str().map(str::to_owned);
            json!({"ok": true})
        }
        // The same for `allow` (the digest without the terminal rule).
        #[cfg(test)]
        Some("test_allow") => allow(shared, request),
        _ => refuse("badRequest", "unknown operation"),
    }
}

/// `close_login` (T6): asks the app to leave the link page for the list, which ends a login that
/// waits at its code or its emoji. Refused when no login waits there. The answer says whether the
/// app took the page change (a step must not say "closed" when it did not).
fn close_login(shared: &Arc<Mutex<Shared>>) -> Value {
    let waiting = {
        let s = locked(shared);
        s.stdin.is_some()
            && s.judge
                .login
                .as_ref()
                .is_some_and(|(step, kind)| matches!(step.as_str(), "qr" | "emoji") && kind == step)
    };
    if !waiting {
        return refuse(
            "notWaiting",
            "no login waits at its code or its emoji: nothing was closed",
        );
    }
    let closed = matches!(
        app_call(shared, "page", &json!({"name": "list"}), Duration::from_secs(20)),
        Ok(answer) if answer["ok"] == json!(true)
    );
    locked(shared)
        .push_event(json!({"type": "event", "event": "login_closed", "data": {"ok": closed}}));
    json!({"ok": true, "closed": closed})
}

/// The TEST phone's My ID as a digest, read by the supervisor itself (B1).
fn digest_from_the_phone(shared: &Arc<Mutex<Shared>>) -> Result<String, Value> {
    let (dir, serial, opener) = {
        let s = locked(shared);
        let Some(serial) = s.state.test_phone().map(str::to_owned) else {
            return Err(refuse(
                "noTestPhone",
                "this run has no test phone: the ID is typed in `threema-run id`. Nothing was taken",
            ));
        };
        (s.dir.clone(), serial, Arc::clone(&s.phones))
    };
    let mut phone = opener(&dir, &serial).map_err(|_| {
        refuse(
            "phoneUnreachable",
            "the TEST phone could not be reached: nothing was read",
        )
    })?;
    super::tphone::own_id_digest(phone.as_mut()).map_err(|_| {
        refuse(
            "idUnreadable",
            "the TEST phone's My ID could not be read as an 8-character ID: nothing was taken",
        )
    })
}

fn allow_from_phone(shared: &Arc<Mutex<Shared>>, request: &Value) -> Value {
    // The gates first (a start that waits for the ID, no open question), THEN the phone.
    {
        let s = locked(shared);
        if s.state.test_phone().is_none() {
            return refuse(
                "noTestPhone",
                "this run has no test phone: the ID is typed in `threema-run id`. Nothing was taken",
            );
        }
        if !s.for_link || s.id != IdState::Waiting {
            return refuse(
                "notWaiting",
                "this start does not wait for the ID (it was started without --for-link, the ID was taken already, or it gave up waiting): nothing was taken",
            );
        }
    }
    if request.get("digest").is_some() {
        return refuse(
            "badRequest",
            "this operation takes no digest: the supervisor reads the TEST phone itself. Nothing was taken",
        );
    }
    let mut digest = match digest_from_the_phone(shared) {
        Ok(d) => d,
        Err(refusal) => return refusal,
    };
    let answer = allow_digest(shared, &digest);
    let mut bytes = std::mem::take(&mut digest).into_bytes();
    wipe(&mut bytes);
    answer
}

/// `hand`: whether the TEST phone's My ID is the TEST ID the run holds a digest of.
fn hand(shared: &Arc<Mutex<Shared>>) -> Value {
    let mut digest = match digest_from_the_phone(shared) {
        Ok(d) => d,
        Err(refusal) => return refusal,
    };
    let (known, same) = {
        let s = locked(shared);
        (
            s.allowed_digest.is_some(),
            s.allowed_digest.as_deref() == Some(digest.as_str()),
        )
    };
    let mut bytes = std::mem::take(&mut digest).into_bytes();
    wipe(&mut bytes);
    json!({"ok": true, "known": known, "same": same})
}

/// `allow` (T4, from `threema-run id` on a terminal): the digest of the test ID, never the ID. Taken
/// once, only by a `--for-link` start that waits, never while the question of a failed link stands;
/// the list is written under the keeper lock (held until it is gone), then the gate opens and the
/// app starts. The answer echoes nothing of the request.
fn allow(shared: &Arc<Mutex<Shared>>, request: &Value) -> Value {
    let Some(digest) = request["digest"].as_str().filter(|d| is_digest_hex(d)) else {
        return refuse(
            "badRequest",
            "a digest is 64 lower-case hex digits: nothing was taken",
        );
    };
    allow_digest(shared, digest)
}

/// The list for one digest (from a terminal's `allow`, or from the supervisor's own read of the TEST
/// phone): the gates, then the file.
fn allow_digest(shared: &Arc<Mutex<Shared>>, digest: &str) -> Value {
    let mut s = locked(shared);
    if !s.for_link || s.id != IdState::Waiting {
        return refuse(
            "notWaiting",
            "this start does not wait for the ID (it was started without --for-link, the ID was taken already, or it gave up waiting): nothing was taken",
        );
    }
    if question_kind(&s.dir).is_some() {
        return refuse(
            "questionOpen",
            "the question of a failed link is open in the keeper root: answer it first (`threema-run answer`). Nothing was taken",
        );
    }
    // LOW-3: the list is read by the app of ANY direct child of the keeper root (the app takes every
    // one as a run profile), and the only look at the other profiles was the start's, minutes ago:
    // nothing is written while a Kourier or a supervisor runs on one of them.
    match other_runs_on_the_keeper_root(&s.dir) {
        Ok(None) => {}
        Ok(Some(why)) => return refuse("otherKourierRunning", why),
        Err(_) => {
            return refuse(
                "otherKourierRunning",
                "cannot tell whether a Kourier runs on another profile of the keeper root: nothing was written",
            );
        }
    }
    match write_the_list(&s.dir, digest) {
        Ok(lock) => {
            s.keeper_lock = Some(lock);
            s.allowed_digest = Some(digest.to_owned());
            s.list = LIST_STANDS;
            s.push_event(
                json!({"type": "event", "event": "allow_list", "data": {"written": true}}),
            );
            s.id_arrived();
            json!({"ok": true})
        }
        Err(e) => {
            eprintln!("threema-run supervisor: the list of test IDs was not written ({e:#})");
            refuse(
                "listNotWritten",
                "the list of test IDs was not written (another run holds the keeper lock, or the keeper root cannot be written): nothing was taken",
            )
        }
    }
}

/// Whether a Kourier or a supervisor runs on a direct child of the keeper root other than this
/// profile (its supervisor lock is this process's own), or the app on this one: LOW-3. EVERY direct
/// directory counts, whatever its name (the app takes any child as a run profile, `backups` and a
/// hand-made `rt1.old` too). `Some(words)` when one does (no name in the words: a name is the user's);
/// an error when a lock cannot be looked at (the caller refuses what it cannot see).
fn other_runs_on_the_keeper_root(dir: &ProfileDir) -> Result<Option<&'static str>> {
    for entry in std::fs::read_dir(dir.keeper())? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let child = ProfileDir {
            dir: entry.path(),
            rehearsal: dir.rehearsal,
        };
        if probe_lock(&app_lock_file(&child), LOCK_LOOK)? == Lock::Held {
            return Ok(Some(
                "a Kourier runs on a profile of the keeper root: it would read the list of test IDs. Nothing was written; stop it, then type the ID again",
            ));
        }
        if child.dir != dir.dir && probe_lock(&child.lock_file(), LOCK_LOOK)? == Lock::Held {
            return Ok(Some(
                "another threema-run supervisor holds a profile of the keeper root. Nothing was written; stop it, then type the ID again",
            ));
        }
    }
    Ok(None)
}

/// `answer` (T4, from `threema-run answer` on a terminal): his y or n to the question a step put, or
/// to the question of a failed link in the keeper root (S1b): that one is closed here, the state
/// first (`link_at` spent, the answer recorded), then the file.
fn answer(shared: &Arc<Mutex<Shared>>, request: &Value) -> Value {
    answer_with(shared, request, "typed")
}

fn answer_with(shared: &Arc<Mutex<Shared>>, request: &Value, origin: &'static str) -> Value {
    let name = request["name"].as_str().unwrap_or_default();
    let Some(yes) = request["yes"].as_bool() else {
        return refuse("badRequest", "an answer is a yes or a no");
    };
    let mut s = locked(shared);
    if name == QUESTION_KIND {
        let kind = question_kind(&s.dir);
        if kind.is_none() || (kind.as_deref() == Some(LINKING_KIND) && s.state.link_stood()) {
            return refuse(
                "notAsked",
                "no question of a failed link is open (a login of this run that stands at linking is not one yet)",
            );
        }
        let mut state = s.state.clone();
        record_link_answer(&mut state, yes, now_ms());
        if let Err(e) = state.save(&s.dir) {
            eprintln!("threema-run supervisor: the answer could not be recorded ({e:#})");
            return refuse(
                "stateUnwritable",
                "the answer could not be recorded: the question stays open",
            );
        }
        s.state = state;
        if let Err(e) = remove_synced(&s.dir.open_question_file()) {
            eprintln!("threema-run supervisor: the question could not be removed ({e:#})");
            return refuse(
                "questionStays",
                "the question's file could not be removed: it stays open",
            );
        }
        s.push_event(
            json!({"type": "event", "event": "question_answered", "data": {"name": name, "yes": yes}}),
        );
        return json!({"ok": true});
    }
    if s.question.as_deref() != Some(name) {
        return refuse(
            "notAsked",
            "no such question is open: an answer is taken only for a question a step asked",
        );
    }
    s.question = None;
    s.answer = Some((name.to_owned(), yes));
    s.answer_origin = origin;
    s.push_event(
        json!({"type": "event", "event": "question_answered", "data": {"name": name, "yes": yes}}),
    );
    json!({"ok": true})
}

/// Forwards one dev-control command of the allow-list ([`CTL_FORWARDED`]) to the app and returns its
/// answer, the result redacted. Commands are recorded by name only (their arguments can carry an
/// id); a halted or ending run forwards reading only.
fn ctl(shared: &Arc<Mutex<Shared>>, request: &Value) -> Value {
    let Some(cmd) = request["cmd"].as_str() else {
        return refuse("badRequest", "ctl without cmd");
    };
    let args = request.get("args").cloned().unwrap_or_else(|| json!({}));
    let timeout = Duration::from_millis(
        request["timeout_ms"]
            .as_u64()
            .unwrap_or(120_000)
            .min(600_000),
    );
    {
        let s = locked(shared);
        let allowed: &[&str] = if s.halted().is_some() || s.ending {
            &CTL_WHEN_HALTED
        } else {
            &CTL_FORWARDED
        };
        let cue =
            s.dir.rehearsal && s.halted().is_none() && !s.ending && rehearsal_cue_ok(cmd, &args);
        if !allowed.contains(&cmd) && !cue {
            // The command's name is not echoed: it came from a client, and a name could be anything.
            return refuse(
                "refused",
                "that command is not forwarded to the app in this run (add_account, send, act and attach never are, but the cues of a rehearsal's fake phone; a halted or ending run forwards reading only)",
            );
        }
    }
    forward(shared, cmd, &args, timeout)
}

/// One command to the app, recorded by name only (its arguments can carry an id), the answer redacted.
fn forward(shared: &Arc<Mutex<Shared>>, cmd: &str, args: &Value, timeout: Duration) -> Value {
    let started = Instant::now();
    let response = match app_call(shared, cmd, args, timeout) {
        Ok(v) => v,
        Err(refusal) => return refusal,
    };
    let ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let ok = response["ok"] == json!(true);
    let mut record =
        json!({"type": "event", "event": "ctl", "data": {"cmd": cmd, "ok": ok, "ms": ms}});
    if !ok {
        record["data"]["code"] = json!(word_of(&response["error"]["code"]));
    }
    locked(shared).push_event(record);
    if ok {
        json!({"ok": true, "result": redact_value(&response["result"])})
    } else {
        let message = response["error"]["message"].as_str().unwrap_or("failed");
        refuse(&word_of(&response["error"]["code"]), &redact(message))
    }
}

/// [`forward`] as a `Result`: the result, or the ready refusal.
fn forwarded(
    shared: &Arc<Mutex<Shared>>,
    cmd: &str,
    args: &Value,
    timeout: Duration,
) -> std::result::Result<Value, Value> {
    let answer = forward(shared, cmd, args, timeout);
    if answer["ok"] == json!(true) {
        Ok(answer["result"].clone())
    } else {
        Err(answer)
    }
}

/// Polls the app's `state` until `holds`; `Err` is a ready refusal (`why`), also when the run halts.
fn state_becomes(
    shared: &Arc<Mutex<Shared>>,
    bound: Duration,
    why: &str,
    holds: impl Fn(&Value) -> bool,
) -> std::result::Result<(), Value> {
    let limit = Instant::now() + bound;
    loop {
        {
            let s = locked(shared);
            if s.halted().is_some() || s.ending {
                return Err(refuse(
                    "halted",
                    "the run is halted or ending: nothing was sent",
                ));
            }
        }
        let state = forward(shared, "state", &json!({}), Duration::from_secs(10));
        if state["ok"] == json!(true) && holds(&state["result"]) {
            return Ok(());
        }
        if Instant::now() >= limit {
            return Err(refuse("timeout", why));
        }
        std::thread::sleep(STEP_SEND_POLL);
    }
}

/// The tool's own picture for `send-file`, made here: no client names a file, so nothing of the
/// user's can ride along.
fn write_step_picture(path: &Path) -> Result<()> {
    let file = std::fs::File::create(path)?;
    let mut encoder = png::Encoder::new(
        std::io::BufWriter::new(file),
        STEP_PICTURE_SIDE,
        STEP_PICTURE_SIDE,
    );
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header()?;
    let shade = |v: u32| u8::try_from(v * 2).unwrap_or(u8::MAX);
    let data: Vec<u8> = (0..STEP_PICTURE_SIDE)
        .flat_map(|y| (0..STEP_PICTURE_SIDE).flat_map(move |x| [shade(x), shade(y), 160]))
        .collect();
    writer.write_image_data(&data)?;
    writer.finish()?;
    Ok(())
}

/// What comes before a step's own send: the chat open, and for `picture` the file in its composer,
/// looked at by the core. `Ok` is the number of files the send must name; nothing was sent on `Err`.
fn step_send_ready(
    shared: &Arc<Mutex<Shared>>,
    conversation: i64,
    picture: Option<&Path>,
) -> std::result::Result<i32, Value> {
    forwarded(
        shared,
        "open_id",
        &json!({"conversation": conversation}),
        STEP_SEND_CHAT_WITHIN,
    )?;
    state_becomes(
        shared,
        STEP_SEND_CHAT_WITHIN,
        "the chat did not open: nothing was sent",
        |state| state["open"] == json!(conversation),
    )?;
    let Some(path) = picture else {
        return Ok(0);
    };
    write_step_picture(path).map_err(|_| {
        refuse(
            "failed",
            "the tool's picture was not written: nothing was sent",
        )
    })?;
    forwarded(
        shared,
        "attach",
        &json!({"path": path.display().to_string()}),
        STEP_SEND_CHAT_WITHIN,
    )?;
    state_becomes(
        shared,
        STEP_SEND_FILE_WITHIN,
        "the picture did not get ready to send: nothing was sent",
        |state| {
            let c = &state["composer"];
            c["planned"] == json!(true)
                && c["preparing"] == json!(0)
                && c["files"] == json!(1)
                && c["sendable"] == json!(1)
                && c["refused"] == json!(0)
        },
    )?;
    Ok(1)
}

/// A send step's own send (`step <name> --lead-sends`, the user's word in chat; [`serve`] takes it
/// only from that step's process). One thing goes out: the step's marker, into the chat it names,
/// with the tool's own picture on `send-file`; only while the step's window is open and the run
/// stands, and once per step and start, whatever the step remembers. The app checks on its Qt thread
/// that the open chat is the named one and that the composer holds nothing else.
fn step_send(shared: &Arc<Mutex<Shared>>, request: &Value) -> Value {
    let what = request["what"].as_str().unwrap_or_default();
    let text = request["text"].as_str().unwrap_or_default();
    let conversation = request["conversation"].as_i64().unwrap_or(0);
    let picture = request["picture"] == json!(true);
    if !SEND_WINDOWS.contains(&what) {
        return refuse("badRequest", "that is not a send step");
    }
    if !is_marker_for(what, text) || conversation <= 0 || picture != (what == "send-file") {
        return refuse(
            "badRequest",
            "a step's own send is its marker into a named chat, with a picture on `send-file` only: nothing was sent",
        );
    }
    let picture_at = {
        let mut s = locked(shared);
        if s.halted().is_some() || s.ending {
            return refuse("halted", "the run is halted or ending: nothing was sent");
        }
        if !s.judge.send.is_open() {
            return refuse(
                "no-window",
                "the step's send window is not open: nothing was sent",
            );
        }
        if !s.judge.send.claim(what) {
            return refuse(
                "sent-before",
                "this start already made this step's send: it is never made twice",
            );
        }
        s.push_event(json!({"type": "event", "event": "step_send", "data": {"what": what}}));
        picture.then(|| s.dir.tool().join(STEP_PICTURE))
    };
    let files = match step_send_ready(shared, conversation, picture_at.as_deref()) {
        Ok(files) => files,
        Err(refusal) => {
            locked(shared).judge.send.give_back(what);
            return refusal;
        }
    };
    // Claimed for good from here: a send whose answer is lost is not made again.
    forward(
        shared,
        "send",
        &json!({"text": text, "conversation": conversation, "files": files}),
        STEP_SEND_ANSWER_WITHIN,
    )
}

/// Sends one command to the app and waits for its response (wa_run.rs:4637 `app_call`). `Err` is a
/// ready refusal for the client.
fn app_call(
    shared: &Arc<Mutex<Shared>>,
    cmd: &str,
    args: &Value,
    timeout: Duration,
) -> std::result::Result<Value, Value> {
    let (id, rx) = {
        let mut s = locked(shared);
        s.next_id += 1;
        let id = s.next_id;
        let (tx, rx) = mpsc::channel();
        let line = json!({"id": id, "cmd": cmd, "args": args});
        let Some(stdin) = s.stdin.as_mut() else {
            return Err(refuse("gone", "the app is not running or is stopping"));
        };
        if writeln!(stdin, "{line}")
            .and_then(|()| stdin.flush())
            .is_err()
        {
            return Err(refuse("gone", "the app does not take commands any more"));
        }
        s.pending.insert(id, tx);
        (id, rx)
    };
    match rx.recv_timeout(timeout) {
        Ok(v) => Ok(v),
        Err(RecvTimeoutError::Timeout) => {
            locked(shared).pending.remove(&id);
            Err(refuse("timeout", "the app did not answer in time"))
        }
        Err(RecvTimeoutError::Disconnected) => Err(refuse("gone", "the app exited")),
    }
}

/// Serves one connection: one line in, one line out (wa_run.rs:5019 `serve`).
fn serve(shared: &Arc<Mutex<Shared>>, stream: UnixStream) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let Ok(read_half) = stream.try_clone() else {
        return;
    };
    let mut line = Vec::new();
    let limit = u64::try_from(REQUEST_MAX + 1).unwrap_or(u64::MAX);
    if BufReader::new(read_half.take(limit))
        .read_until(b'\n', &mut line)
        .is_err()
    {
        return;
    }
    let answer = if line.len() > REQUEST_MAX {
        refuse("badRequest", "the request is too long")
    } else {
        let request: Value = serde_json::from_slice(&line).unwrap_or(Value::Null);
        // His y or n and the digest of the test ID come from a process in the foreground of a
        // terminal, whoever else can write to the socket (wa_run's T2 F5).
        if needs_a_terminal(request["op"].as_str()) && !peer_is_on_a_terminal(&stream) {
            refuse("no-terminal", NOT_FROM_A_TERMINAL)
        } else if matches!(
            request["op"].as_str(),
            Some("send_window" | "send_window_over")
        ) && !peer_is_the_step(&stream, request["what"].as_str().unwrap_or_default())
        {
            // The window lifts the no-send rule: only the `step` of that name says so.
            refuse(
                "no-step",
                "a send window is asked by the step of that name only (`threema-run step send`, `group-send` or `send-file`): nothing was changed",
            )
        } else if request["op"].as_str() == Some("step_send")
            && !peer_is_the_sending_step(&stream, request["what"].as_str().unwrap_or_default())
        {
            // The tool's own send: only the `step` of that name, and only with the user's word.
            refuse(
                "no-step",
                "a step's own send is asked by the step of that name with --lead-sends only: nothing was sent",
            )
        } else if matches!(request["op"].as_str(), Some("removal" | "removal_over"))
            && !peer_is_the_step(&stream, request["what"].as_str().unwrap_or_default())
        {
            // LOW-9: a removal asked tells the judge that 4113, `logged_out` and the drop frame are
            // no halts: only the `step <what>` of this tool says so.
            refuse(
                "no-step",
                "a removal is asked by the step of that name only (`threema-run step unlink` or `removed-on-phone`): nothing was changed",
            )
        } else {
            handle(shared, &request)
        }
    };
    let mut stream = stream;
    let _ = writeln!(stream, "{answer}");
}

/// Whether `cmdline` (`/proc/<pid>/cmdline`: the arguments, each ended by a NUL) is this tool's step
/// `what`: `<program> threema-run step <what> ...` (LOW-9).
pub(super) fn is_the_step_cmdline(cmdline: &[u8], what: &str) -> bool {
    let args: Vec<&[u8]> = cmdline
        .split(|b| *b == 0)
        .filter(|arg| !arg.is_empty())
        .collect();
    matches!(args.get(1..4), Some([a, b, c]) if *a == b"threema-run" && *b == b"step" && *c == what.as_bytes())
}

/// Whether the process at the other end of the connection runs the step `what` of this tool: its
/// `/proc/<pid>/cmdline` says so (peer credentials, as the terminal rule reads them). A process that
/// is gone before it is looked at is not. In a unit test the test process itself counts: the step
/// client runs in it (the refusal is tested with a child process of another command line).
fn peer_is_the_step(stream: &UnixStream, what: &str) -> bool {
    peer_cmdline_is(stream, |cmdline| is_the_step_cmdline(cmdline, what))
}

/// Whether `cmdline` is the step `what` of this tool with `--lead-sends` (the user's word in chat).
pub(super) fn is_the_sending_step_cmdline(cmdline: &[u8], what: &str) -> bool {
    is_the_step_cmdline(cmdline, what) && cmdline.split(|b| *b == 0).any(|a| a == b"--lead-sends")
}

fn peer_is_the_sending_step(stream: &UnixStream, what: &str) -> bool {
    peer_cmdline_is(stream, |cmdline| is_the_sending_step_cmdline(cmdline, what))
}

fn peer_cmdline_is(stream: &UnixStream, is: impl Fn(&[u8]) -> bool) -> bool {
    let Some(pid) = peer_pid(stream) else {
        return false;
    };
    #[cfg(test)]
    if pid == std::process::id() {
        return true;
    }
    std::fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|cmdline| is(&cmdline))
}

fn serve_forever(listener: &UnixListener, shared: &Arc<Mutex<Shared>>) {
    for stream in listener.incoming().flatten() {
        let shared = Arc::clone(shared);
        std::thread::spawn(move || serve(&shared, stream));
    }
}

// ---- The client ---------------------------------------------------------------------------------------

/// What a refused request says (the supervisor's `error`).
#[derive(Debug)]
pub(super) struct Refused {
    pub code: String,
    pub message: String,
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for Refused {}

/// One request over the control socket: one JSON line in, one out. An `ok: false` answer is a
/// [`Refused`].
pub(super) fn call_at(socket: &Path, request: &Value, timeout: Duration) -> Result<Value> {
    let mut stream = UnixStream::connect(socket).context("connect to the control socket")?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    writeln!(stream, "{request}")?;
    let mut line = String::new();
    BufReader::new(stream)
        .read_line(&mut line)
        .context("the supervisor did not answer")?;
    let answer: Value = serde_json::from_str(&line).context("the supervisor's answer")?;
    if answer["ok"] == json!(true) {
        return Ok(answer);
    }
    Err(Refused {
        code: answer["error"]["code"]
            .as_str()
            .unwrap_or("failed")
            .to_owned(),
        message: answer["error"]["message"]
            .as_str()
            .unwrap_or("failed")
            .to_owned(),
    }
    .into())
}

// ---- The app's two outputs --------------------------------------------------------------------------

/// Reads `reader` line by line as bytes (a line that is not UTF-8 must not end the reading: an app
/// whose pipe nobody empties stops at its next write).
fn for_each_line(reader: impl std::io::Read, mut each: impl FnMut(&str)) {
    let mut reader = BufReader::new(reader);
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        each(String::from_utf8_lossy(&line).trim_end_matches(['\n', '\r']));
    }
}

/// The app's log: every stderr line through the shared state ([`Shared::on_app_line`]: collected raw
/// in memory, written redacted). The pipe is read to its end.
fn read_app_stderr(stderr: ChildStderr, shared: &Arc<Mutex<Shared>>) {
    for_each_line(stderr, |line| locked(shared).on_app_line(line));
}

/// The dev channel: the responses to whoever asked, the events into the ring.
fn read_app_events(stdout: ChildStdout, shared: &Arc<Mutex<Shared>>) {
    for_each_line(stdout, |line| {
        let Ok(v @ Value::Object(_)) = serde_json::from_str::<Value>(line) else {
            return;
        };
        let mut s = locked(shared);
        match v["type"].as_str() {
            Some("response") => {
                s.on_first_answer();
                if let Some(tx) = v["id"].as_u64().and_then(|id| s.pending.remove(&id)) {
                    let _ = tx.send(v);
                }
            }
            Some("event") => s.on_app_event(v),
            _ => {}
        }
    });
}

/// What an end (a stop or a halt) does to the app, once, off the lock (it talks to the app), in this
/// order (tr.md §2.10, critic M2):
///
/// 1. a link the phone confirmed (the login at `linking`) is let finish, up to [`LINK_END_WAIT`];
/// 2. a link that failed after its hello: the core's logout drops the device now, and RP-b's outcome
///    line (D4) is waited for up to [`DROP_WAIT`] from the failed step (a `quit` before it would cancel
///    the drop); the outcome goes into the halt's record and the question;
/// 3. a login that waits at its code (`qr`, `emoji`) is closed by the page change to the list;
/// 4. the app is asked to quit; the loop kills it after the grace. The switch goes off after its
///    exit (the supervisor's end), the list of test IDs with it.
fn end_the_run(shared: &Arc<Mutex<Shared>>) {
    // Each look takes the lock alone and lets it go (a guard of one look must not stand while the
    // next is taken).
    let poll = Duration::from_millis(20);
    let link_end = Instant::now() + locked(shared).link_end;
    loop {
        let in_flight = link_in_flight(&locked(shared));
        if !in_flight || Instant::now() >= link_end {
            break;
        }
        std::thread::sleep(poll);
    }
    let owed = {
        let s = locked(shared);
        s.judge.drop_owed_since_ms.map(|since| (since, s.drop_wait))
    };
    if let Some((since, wait)) = owed {
        let bound = u64::try_from(wait.as_millis()).unwrap_or(u64::MAX);
        loop {
            let settled = {
                let s = locked(shared);
                s.judge.drop.is_some() || s.exit.is_some()
            };
            if settled || now_ms().saturating_sub(since) >= bound {
                break;
            }
            std::thread::sleep(poll);
        }
        locked(shared).note_drop();
    }
    let waiting = locked(shared)
        .judge
        .login
        .as_ref()
        .is_some_and(|(step, kind)| matches!(step.as_str(), "qr" | "emoji") && kind == step);
    if waiting {
        let closed = matches!(
            app_call(shared, "page", &json!({"name": "list"}), Duration::from_secs(20)),
            Ok(answer) if answer["ok"] == json!(true)
        );
        locked(shared)
            .push_event(json!({"type": "event", "event": "login_closed", "data": {"ok": closed}}));
    }
    let mut s = locked(shared);
    s.quit();
    s.kill_requested = true;
}

/// A link the phone confirmed is under way: the login stands at `linking` (not failed, not done).
fn link_in_flight(s: &Shared) -> bool {
    s.exit.is_none()
        && s.judge
            .login
            .as_ref()
            .is_some_and(|(step, kind)| step == "linking" && kind != "failed" && kind != "done")
}

/// Starts [`end_the_run`] when an end has been raised (a stop or a halt), once. Looks every `every`
/// until `over` is set.
fn watch_for_ending(shared: &Arc<Mutex<Shared>>, over: &AtomicBool, every: Duration) {
    while !over.load(Ordering::Relaxed) {
        let start = {
            let mut s = locked(shared);
            let start = s.ending && !s.ending_started;
            if start {
                s.ending_started = true;
            }
            start
        };
        if start {
            let ending = Arc::clone(shared);
            std::thread::spawn(move || end_the_run(&ending));
        }
        std::thread::sleep(every);
    }
}

// ---- The process --------------------------------------------------------------------------------------

/// `prctl(PR_SET_DUMPABLE, 0)`: no core file, no `ptrace`, no `/proc/<pid>/mem` for the user's other
/// processes (wa_run.rs:2233 `make_non_dumpable`).
pub(super) fn make_non_dumpable() -> Result<()> {
    // SAFETY: `prctl` with `PR_SET_DUMPABLE` takes plain integers and changes this process's flag
    // only.
    if unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) } != 0 {
        bail!(
            "cannot make this process non-dumpable ({}): a core file could hold the account's keys. Nothing was started.",
            std::io::Error::last_os_error()
        );
    }
    Ok(())
}

/// Whether this process is dumpable (`PR_GET_DUMPABLE` is 1).
fn is_dumpable() -> bool {
    // SAFETY: `PR_GET_DUMPABLE` only reads this process's flag.
    unsafe { libc::prctl(libc::PR_GET_DUMPABLE, 0, 0, 0, 0) == 1 }
}

/// Set by the signal handler: the supervisor was told to end (SIGTERM, SIGINT, SIGHUP).
static TERMINATED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_terminate(_signal: libc::c_int) {
    TERMINATED.store(true, Ordering::Relaxed);
}

/// A signal that would kill the supervisor ends the run in order instead: the app is asked to quit,
/// waited for, and only then is the switch written off (critic M3). SIGKILL cannot be caught.
fn install_signal_handlers() {
    for signal in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
        // SAFETY: the handler only stores into an atomic, which is async-signal-safe.
        unsafe {
            libc::signal(signal, on_terminate as *const () as libc::sighandler_t);
        }
    }
}

/// The secret files of a process that the stamped copy's mode keeps from this user (wa_run.rs:2721).
const SECRET_FILES: [&str; 2] = ["environ", "smaps_rollup"];

/// Whether this user can read `path` (a read of its first byte; wa_run.rs:4184).
pub(super) fn proc_file_readable(path: &Path) -> bool {
    std::fs::File::open(path).is_ok_and(|mut f| f.read(&mut [0u8; 1]).is_ok())
}

/// The secret files of process `pid` that `readable` can read: none when the process is protected
/// (wa_run.rs:4190).
pub(super) fn readable_files(pid: u32, readable: &dyn Fn(&Path) -> bool) -> Vec<&'static str> {
    let base = PathBuf::from("/proc").join(pid.to_string());
    SECRET_FILES
        .iter()
        .copied()
        .filter(|name| readable(&base.join(name)))
        .collect()
}

/// The child process of the app. Dropped while it still runs (a panic, an error), it is killed and
/// reaped, so the end that follows (the switch off, the lock) never meets a live app.
struct ChildGuard {
    child: Child,
    reaped: bool,
}

impl ChildGuard {
    fn try_wait(&mut self) -> Result<Option<std::process::ExitStatus>> {
        let status = self.child.try_wait()?;
        self.reaped |= status.is_some();
        Ok(status)
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if !self.reaped {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// The end of a run, written once the app's process is gone and reaped: the list of test IDs of a
/// link start (belt), then the switch off in the file as it is NOW. Dropped unfinished (an error, a
/// panic) it does the same; the profile lock is released after it.
struct EndGuard<'a> {
    dir: &'a ProfileDir,
    for_link: bool,
    done: bool,
}

impl EndGuard<'_> {
    fn finish(&mut self) -> Result<bool> {
        self.done = true;
        // The belt: a list of this run that is still there (the run's own lock went with it; a list
        // under another run's live lock is that run's and stays).
        if self.for_link && remove_the_list(self.dir, None).is_err() {
            eprintln!(
                "threema-run supervisor: the list of test IDs could not be removed: remove it by hand, it stands in the keeper root"
            );
            // LOW-6: the run's record says so (the first cause stays).
            if let Ok(Some(mut state)) = State::load(self.dir)
                && state.halt_kind().is_none()
            {
                state.set(
                    "halted",
                    json!({"kind": "list-not-removed", "at_ms": now_ms()}),
                );
                let _ = state.save(self.dir);
            }
        }
        switch_off(self.dir)
    }
}

impl Drop for EndGuard<'_> {
    fn drop(&mut self) {
        if !self.done
            && let Err(e) = self.finish()
        {
            eprintln!(
                "threema-run supervisor: the switch could not be written off ({e:#}): run `threema-run stop` before anything else"
            );
        }
    }
}

/// What the supervisor takes from outside (a test replaces them).
pub(super) struct Seams<'a> {
    /// This user's id (the order's socket directory must be this user's).
    pub uid: u32,
    pub now: &'a dyn Fn() -> u64,
    /// The check of the stamped copy at boot.
    pub check: &'a dyn Fn(&ProfileDir) -> Result<Stamp>,
    /// The command that starts the app, built at the moment it starts (the copy is looked at again:
    /// the wait for the ID can take minutes).
    pub make: &'a dyn Fn(&Order) -> Result<Command>,
    pub timing: &'a Timing,
    /// Set when the supervisor is told to end by a signal.
    pub terminated: &'a AtomicBool,
    /// LOW-7: whether this user can read the file `path` (a read of its first byte), asked of the
    /// app's `/proc/<pid>/environ` and `smaps_rollup` right after its start.
    pub readable: &'a dyn Fn(&Path) -> bool,
    /// How the TEST phone is opened (B1: the supervisor reads it itself).
    pub phone: PhoneOpener,
}

/// `threema-run __supervise`, started by `start` and by nothing else.
pub(super) fn supervise(a: &Args) -> Result<i32> {
    make_non_dumpable()?;
    install_signal_handlers();
    let dir = a.place.resolve(&var, &target_dir())?;
    let inherited: Vec<OsString> = std::env::vars_os().map(|(key, _)| key).collect();
    let make = |order: &Order| -> Result<Command> {
        check_copy(&dir)?;
        Ok(app_command(&dir.copy_file(), &dir, order, &inherited))
    };
    // SAFETY: `geteuid` has no arguments and cannot fail.
    let uid = unsafe { libc::geteuid() };
    supervise_in(
        &dir,
        &Seams {
            uid,
            now: &now_ms,
            check: &check_copy,
            make: &make,
            timing: &Timing::REAL,
            terminated: &TERMINATED,
            readable: &proc_file_readable,
            phone: Arc::new(super::tphone::open_phone),
        },
    )
}

/// The supervisor after its checks, in the module's order. The profile lock is the FIRST thing taken
/// and the LAST given back; the end writes the switch off after the app is gone.
pub(super) fn supervise_in(dir: &ProfileDir, seams: &Seams<'_>) -> Result<i32> {
    let lock = take_profile_lock(dir)?;
    // No order, no start: whoever started this by hand starts nothing, and the switch is the
    // start's own guard's.
    let order = take_launch(dir, (seams.now)(), seams.uid)?;
    let mut end = EndGuard {
        dir,
        for_link: order.for_link,
        done: false,
    };
    let stamp = (seams.check)(dir)?;
    let state = State::load(dir)?.context("no state file: `start` makes it")?;
    let result = run_supervised(dir, &order, state, &stamp, seams);
    // The app is gone and reaped, or was never started: now the switch, then the lock.
    let off = end.finish();
    drop(lock);
    let code = result?;
    off?;
    Ok(code)
}

/// The socket, the wait for the ID, the app, the loop until it is gone (every exit leaves no socket
/// and no `supervisor.json`).
fn run_supervised(
    dir: &ProfileDir,
    order: &Order,
    state: State,
    stamp: &Stamp,
    seams: &Seams<'_>,
) -> Result<i32> {
    let listener = listen_at(&order.socket)?;
    let shared = Arc::new(Mutex::new(Shared::new(
        dir,
        order,
        state,
        stamp,
        seams.timing,
    )));
    locked(&shared).phones = Arc::clone(&seams.phone);
    let result = (|| {
        // A state file that cannot be written ends the start before anything runs.
        locked(&shared)
            .state
            .save(dir)
            .context("the state file cannot be written")?;
        write_private(
            &dir.supervisor_file(),
            &json!({"pid": std::process::id(), "socket": order.socket, "app_pid": null})
                .to_string(),
        )?;
        let served = Arc::clone(&shared);
        std::thread::spawn(move || serve_forever(&listener, &served));
        if !wait_for_id(&shared, seams) {
            // Stopped (or gave up) before there was an app: nothing to start, nothing to wait for.
            let mut s = locked(&shared);
            s.push_event(json!({"type": "event", "event": "stopped_before_start", "data": {}}));
            s.save_state();
            return Ok(0);
        }
        serve_the_app(dir, order, &shared, seams)
    })();
    // Every end of a link start: the list goes under the keeper lock this run holds.
    if order.for_link {
        locked(&shared).list_goes(LIST_AT_END);
    }
    let _ = std::fs::remove_file(&order.socket);
    let _ = std::fs::remove_file(dir.supervisor_file());
    result
}

/// A start for a link waits for the ID (`info.id` is `waiting`) until T4's `id` operation says it
/// arrived. `false` when the run was stopped or the wait ran out: then the app is never started.
fn wait_for_id(shared: &Arc<Mutex<Shared>>, seams: &Seams<'_>) -> bool {
    let deadline = Instant::now() + seams.timing.id_wait;
    loop {
        {
            let mut s = locked(shared);
            if seams.terminated.load(Ordering::Relaxed) {
                s.stop_asked = true;
            }
            if s.stop_asked {
                if s.id == IdState::Waiting {
                    s.id = IdState::GaveUp;
                }
                return false;
            }
            match s.id {
                IdState::NotAsked | IdState::Taken => return true,
                IdState::GaveUp => return false,
                IdState::Waiting if Instant::now() >= deadline => {
                    s.id = IdState::GaveUp;
                    s.push_event(
                        json!({"type": "event", "event": "id_wait_ended", "data": {"typed": false}}),
                    );
                    return false;
                }
                IdState::Waiting => {}
            }
        }
        std::thread::sleep(seams.timing.poll);
    }
}

/// Starts the app and keeps it until it is gone and reaped. Returns 1 when the run halted.
fn serve_the_app(
    dir: &ProfileDir,
    order: &Order,
    shared: &Arc<Mutex<Shared>>,
    seams: &Seams<'_>,
) -> Result<i32> {
    let mut cmd = (seams.make)(order)?;
    let mut guard = ChildGuard {
        child: cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("start the app")?,
        reaped: false,
    };
    drop(cmd);
    let pid = guard.child.id();
    let started = Instant::now();
    {
        let mut s = locked(shared);
        s.app_pid = Some(pid);
        s.started_ms = now_ms();
        s.stdin = guard.child.stdin.take();
        let line = format!(
            "==== threema-run: app started {} (pid {pid}) ====",
            utc(s.started_ms)
        );
        s.log.line(&line);
        s.push_event(json!({"type": "event", "event": "app_started", "data": {}}));
        // A stop that came while the app was starting found no channel to ask on.
        if s.stop_asked {
            s.quit();
        }
    }
    write_private(
        &dir.supervisor_file(),
        &json!({"pid": std::process::id(), "socket": order.socket, "app_pid": pid}).to_string(),
    )?;
    let stderr = guard.child.stderr.take().context("the app's stderr")?;
    let stdout = guard.child.stdout.take().context("the app's stdout")?;
    let line_reader = Arc::clone(shared);
    let stderr_reader = std::thread::spawn(move || read_app_stderr(stderr, &line_reader));
    let reader = Arc::clone(shared);
    let events_reader = std::thread::spawn(move || read_app_events(stdout, &reader));
    let over = Arc::new(AtomicBool::new(false));
    let (ending, ending_over) = (Arc::clone(shared), Arc::clone(&over));
    let every = seams.timing.poll;
    std::thread::spawn(move || watch_for_ending(&ending, &ending_over, every));
    // LOW-7: the app's environment and memory map must be unreadable to this user (the copy is
    // execute-only, so the kernel starts it non-dumpable). Not in a rehearsal: the owner of its user
    // namespace reads every process in it, so the question has no answer there.
    if !dir.rehearsal {
        let exposed = readable_files(pid, seams.readable);
        if !exposed.is_empty() {
            let mut s = locked(shared);
            s.push_event(
                json!({"type": "event", "event": "app_readable", "data": {"files": exposed}}),
            );
            s.app_exposed = exposed;
            s.halt("app-readable");
            eprintln!(
                "threema-run supervisor: the app's environment or memory map can be read by other processes of this user although its copy is execute-only: the run is halted (app-readable)"
            );
        }
    }
    let count_watch_ms = u64::try_from(seams.timing.count_watch.as_millis()).unwrap_or(u64::MAX);

    let (mut kill_at, mut last_save, mut not_up) = (None::<Instant>, Instant::now(), false);
    let status = loop {
        if let Some(status) = guard.try_wait()? {
            break status;
        }
        {
            let mut s = locked(shared);
            if seams.terminated.load(Ordering::Relaxed) && !s.stop_asked {
                s.stop_asked = true;
                s.ending = true;
                s.push_event(json!({"type": "event", "event": "signal", "data": {}}));
            }
            // THE TIME LIMIT (critic R-L3): no first answer on the dev channel within the bound.
            if !not_up
                && s.first_answer_ms.is_none()
                && started.elapsed() >= seams.timing.first_answer
            {
                not_up = true;
                s.halt("app-not-up");
                // Nothing to ask to quit: the end thread has nothing to do, the app is killed.
                s.ending_started = true;
                s.kill_now = true;
                eprintln!(
                    "threema-run supervisor: the app gave no answer on its dev control within {} s: it is killed and the run halted (app-not-up)",
                    seams.timing.first_answer.as_secs()
                );
            }
            // MEDIUM-1: a judge that never gets a count line is blind, and a blind judge is no run.
            if !dir.rehearsal
                && !s.ending
                && s.halted().is_none()
                && let Some(kind) = count_lines_missing(&s.judge, now_ms(), count_watch_ms)
            {
                s.halt(&kind);
                eprintln!(
                    "threema-run supervisor: the account is online and no count line came in {} s: the judge is blind, the run is halted ({kind})",
                    seams.timing.count_watch.as_secs()
                );
            }
            if s.kill_now {
                kill_at.get_or_insert_with(Instant::now);
            } else if s.kill_requested && kill_at.is_none() {
                kill_at = Some(Instant::now() + seams.timing.kill_grace);
            }
            // "Last online" follows a connection that stays up, once a minute.
            if s.dirty || (s.online && last_save.elapsed() >= Duration::from_secs(60)) {
                s.save_state();
                last_save = Instant::now();
            }
        }
        if kill_at.is_some_and(|at| Instant::now() >= at) {
            let _ = guard.child.kill();
        }
        std::thread::sleep(seams.timing.poll);
    };
    over.store(true, Ordering::Relaxed);
    // The lines and events the app wrote before it died are still in its pipes (a login step at
    // `linking` just before a crash must not be lost: it is what the next start asks the question
    // on): both are read to their end before the last save.
    let drain = Instant::now() + seams.timing.drain;
    while !(stderr_reader.is_finished() && events_reader.is_finished()) && Instant::now() < drain {
        std::thread::sleep(Duration::from_millis(10));
    }
    let halted = {
        let mut s = locked(shared);
        // Whoever still waits for an answer learns that the app is gone.
        s.pending.clear();
        s.stdin = None;
        let asked = s.stop_asked || s.ending || s.kill_requested || s.kill_now;
        s.exit = Some(status.to_string());
        s.push_event(json!({"type": "event", "event": "app_exited", "data": {"status": status.to_string(), "asked": asked}}));
        let line = format!(
            "==== threema-run: app exited {} ({status}) ====",
            utc(now_ms())
        );
        s.log.line(&line);
        // The app's exit is judged (S8: nobody asked for it, `exited`; S1b: the login stood at
        // `linking`, whoever asked). A login that stood at `linking` keeps its `link_at`: the next
        // start asks the question.
        s.judged(&Seen::AppExited { asked });
        // The keeper root's `linking` of a login whose end was not seen becomes the question (S1b):
        // every link start, `wipe` and `id` refuse until he answers. It says what Kourier's own drop
        // of the device came to (critic M2), when the run saw one.
        let drop = s.judge.drop;
        if s.state.link_stood()
            && question_kind(dir).as_deref() != Some(QUESTION_KIND)
            && let Err(e) = open_question_noting(dir, now_ms(), drop)
        {
            eprintln!("threema-run supervisor: the question could not be opened ({e:#})");
        }
        s.save_state();
        if order.for_link {
            s.list_goes(LIST_AT_END);
        }
        s.halted().is_some()
    };
    Ok(i32::from(halted))
}

// ---- state ------------------------------------------------------------------------------------------

/// The words for a time, or `never`.
fn when(ms: Option<u64>) -> String {
    ms.map_or_else(|| "never".to_owned(), utc)
}

/// `state`: the profile and the running app as counts, words and times, never a value (nothing of the
/// app's log, no path, no name). `socket` is the control socket when one is known.
pub(super) fn state_in(dir: &ProfileDir, socket: Option<&Path>) -> Result<Vec<String>> {
    let mut out = vec![format!("profile:     {}", dir.label())];
    let Some(state) = State::load(dir)? else {
        out.push("tool state:  none (this profile was never started by this tool)".to_owned());
        return Ok(out);
    };
    out.push(format!(
        "tool state:  {} start(s); login since {}; last online {}",
        state.num("starts").unwrap_or(0),
        when(state.num("linked_since_ms")),
        when(state.num("last_online_ms"))
    ));
    match state.halt_kind() {
        Some(kind) => out.push(format!(
            "halted:      {kind} at {} (a new start needs --after-halt and the user's word)",
            when(state.get("halted")["at_ms"].as_u64())
        )),
        None => out.push("halted:      no".to_owned()),
    }
    if state.link_stood() {
        out.push(
            "link:        a login stood at linking and its end was not seen: a device MAY be registered on the TEST phone (the next start asks the question)"
                .to_owned(),
        );
    }
    if let Ok(Some(question)) = super::profile::read_json(&dir.open_question_file()) {
        out.push(format!(
            "question:    open ({}): no link and no wipe until it is answered",
            word_of(&question["kind"])
        ));
    }
    // LOW-2: a list (or its temporary file) in the keeper root. A link start has one for seconds; with
    // no run waiting it is a leftover that `stop` removes.
    if std::fs::symlink_metadata(dir.allow_file()).is_ok()
        || std::fs::symlink_metadata(dir.allow_tmp()).is_ok()
    {
        out.push(
            "list:        a list of test IDs (or the file it is written through) stands in the keeper root: a link start has it for seconds; with no run waiting it is a leftover, and `threema-run stop` removes it"
                .to_owned(),
        );
    }
    let switch = switch_is_on(dir);
    out.push(format!(
        "switch:      {NETWORK_NAME} is {} in the profile",
        if switch { "ON" } else { "off" }
    ));
    let held = |path: &Path| probe_lock(path, LOCK_LOOK).is_ok_and(|l| l == Lock::Held);
    let info = socket.and_then(|s| call_at(s, &json!({"op": "info"}), Duration::from_secs(5)).ok());
    match &info {
        Some(info) => {
            out.push(format!("supervisor:  up (pid {})", info["pid"]));
            out.push(format!(
                "app:         pid {}; dev channel {}; first answer {}",
                info["app_pid"],
                if info["ready"] == json!(true) {
                    "up"
                } else {
                    "not up"
                },
                info["first_answer_ms"]
                    .as_u64()
                    .map_or_else(|| "not yet".to_owned(), |ms| format!("after {ms} ms"))
            ));
            out.push(format!(
                "id:          {}",
                info["id"].as_str().unwrap_or("?")
            ));
            match (
                info["login"]["step"].as_str(),
                info["login"]["kind"].as_str(),
            ) {
                (Some(step), Some(kind)) => out.push(format!("login:       step {step} ({kind})")),
                _ => out.push("login:       none seen".to_owned()),
            }
            match info["account"].as_object() {
                Some(account) => out.push(format!(
                    "account:     {} / {}, device {}; {}",
                    account["state"].as_str().unwrap_or("?"),
                    account["conn"].as_str().unwrap_or("?"),
                    account["device"],
                    if info["online"] == json!(true) {
                        "online"
                    } else {
                        "not online"
                    }
                )),
                None => out.push("account:     none reported".to_owned()),
            }
            let rows = info["lines"]
                .as_object()
                .map(|rows| {
                    rows.iter()
                        .map(|(row, n)| format!("{row} {n}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .filter(|rows| !rows.is_empty())
                .unwrap_or_else(|| "none".to_owned());
            out.push(format!(
                "log:         {} line(s) read, contract lines: {rows}; {} event(s)",
                info["lines_total"], info["next_seq"]
            ));
            if let Some(kind) = info["halted"].as_str() {
                out.push(format!("run:         halted ({kind})"));
            } else if info["ending"] == json!(true) {
                out.push("run:         ending".to_owned());
            }
        }
        None => {
            if held(&dir.lock_file()) {
                out.push(
                    "supervisor:  holds the profile's lock but does not answer (starting, ending or stuck)"
                        .to_owned(),
                );
            } else {
                out.push("supervisor:  not running".to_owned());
                if held(&app_lock_file(dir)) {
                    out.push(
                        "warning:     an app of this profile still runs without a supervisor (its lock is held): run `threema-run stop`"
                            .to_owned(),
                    );
                }
                if switch {
                    out.push(format!(
                        "warning:     {NETWORK_NAME} is switched ON in the profile and no supervisor runs: the last run did not end cleanly. Run `threema-run stop` before anything else."
                    ));
                }
            }
        }
    }
    Ok(out)
}

/// D1: `test phone: <serial> (reachable|unreachable)` for a profile that has one (adb connect and
/// get-state only), and, while the question of a failed link stands, the `ASK-USER` line for the
/// removal that stays the user's.
pub(super) fn test_phone_lines(dir: &ProfileDir) -> Vec<String> {
    let Some(serial) = State::load(dir)
        .ok()
        .flatten()
        .and_then(|s| s.test_phone().map(str::to_owned))
    else {
        return Vec::new();
    };
    let mut out = Vec::new();
    match super::tphone::open_phone(dir, &serial) {
        Ok(_phone) => {
            out.push(format!("test phone:  {serial} (reachable)"));
            // L5: `state` only connects (adb get-state) to say the phone answers; it opens no screen of
            // it. While the question of a failed link stands, the removal on the phone stays his.
            if question_kind(dir).is_some() {
                out.push(
                    format!("ASK-USER [{QUESTION_KIND}]: remove any Kourier entry on the TEST phone's linked devices (the removal stays his), then tell the lead; the lead runs `threema-run answer --relayed y|n --question {QUESTION_KIND}` (y: there was an entry)"),
                );
            }
        }
        Err(_) => out.push(format!("test phone:  {serial} (unreachable)")),
    }
    out
}

/// `state` as the command line runs it.
pub(super) fn state_command(a: &Args) -> Result<i32> {
    let dir = a.place.resolve(&var, &target_dir())?;
    let socket = socket_for_client(&dir, &var).ok();
    for line in state_in(&dir, socket.as_deref())? {
        println!("{line}");
    }
    for line in test_phone_lines(&dir) {
        println!("{line}");
    }
    Ok(0)
}

// ---- stop -------------------------------------------------------------------------------------------

/// How long `stop` waits for what it asked (a test shortens them).
pub(super) struct StopTiming {
    /// For the supervisor to end (it asks the app to quit, waits the grace, kills it, writes the
    /// switch off).
    pub supervisor: Duration,
    /// For the app's own lock to be free.
    pub app: Duration,
    pub poll: Duration,
}

impl StopTiming {
    pub const REAL: Self = Self {
        supervisor: Duration::from_secs(60),
        app: Duration::from_secs(15),
        poll: Duration::from_millis(100),
    };
}

/// Waits until nobody holds the lock file `path` (a missing file is free), at most `bound`.
fn wait_free(path: &Path, bound: Duration, poll: Duration) -> bool {
    let deadline = Instant::now() + bound;
    loop {
        if probe_lock(path, Duration::ZERO).is_ok_and(|l| l == Lock::Free) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(poll);
    }
}

/// `stop`: ends the run (the supervisor asks the app to quit, kills it after the grace and writes
/// the switch off after its exit), waits for the profile lock and for the app's own lock, and
/// writes the switch off itself when nobody runs any more: a killed supervisor leaves it on
/// (critic M3). The login stays. `--force` tells a supervisor that does not end in time to end
/// (SIGTERM: the same orderly end, never a kill of the app from here). Status 0 when nothing runs
/// and the switch is off.
pub(super) fn stop_in(
    dir: &ProfileDir,
    force: bool,
    socket: Option<&Path>,
    timing: &StopTiming,
    term: &dyn Fn(u32),
) -> Result<(Vec<String>, i32)> {
    let mut out = vec![format!("profile:     {}", dir.label())];
    let answers =
        socket.is_some_and(|s| call_at(s, &json!({"op": "info"}), Duration::from_secs(5)).is_ok());
    let alive = answers || probe_lock(&dir.lock_file(), LOCK_LOOK).is_ok_and(|l| l == Lock::Held);
    if alive {
        if answers {
            let asked = socket.is_some_and(|s| {
                call_at(s, &json!({"op": "stop"}), Duration::from_secs(5)).is_ok()
            });
            out.push(if asked {
                "supervisor:  asked to end the app".to_owned()
            } else {
                "supervisor:  did not take the request".to_owned()
            });
        } else {
            out.push("supervisor:  holds the profile's lock and does not answer".to_owned());
        }
        let mut ended = wait_free(&dir.lock_file(), timing.supervisor, timing.poll);
        if !ended && force {
            let pid = super::profile::read_json(&dir.supervisor_file())
                .ok()
                .flatten()
                .and_then(|v| v["pid"].as_u64())
                .and_then(|p| u32::try_from(p).ok())
                .filter(|p| *p > 1);
            match pid {
                Some(pid) => {
                    term(pid);
                    out.push(
                        "supervisor:  --force: told to end (SIGTERM); it ends the app first"
                            .to_owned(),
                    );
                    ended = wait_free(&dir.lock_file(), timing.supervisor, timing.poll);
                }
                None => out.push("supervisor:  --force: its pid is not recorded".to_owned()),
            }
        }
        if !ended {
            out.push(format!(
                "supervisor:  still running: {NETWORK_NAME} stays switched ON until it ends. Look again with `threema-run state`{}.",
                if force { "" } else { ", or stop with --force" }
            ));
            return Ok((out, 1));
        }
        out.push("supervisor:  ended".to_owned());
    }
    // The app: its own lock is the proof it is gone (a killed supervisor leaves a live app for a
    // moment; the switch is written off only after it).
    if !wait_free(&app_lock_file(dir), timing.app, timing.poll) {
        out.push(format!(
            "app:         still running (its lock is held): {NETWORK_NAME} stays switched ON until it is gone. Look again with `threema-run state`."
        ));
        return Ok((out, 1));
    }
    // LOW-2: nothing runs on this profile any more. A supervisor killed between `id` and the app's first
    // answer leaves the list of test IDs in the keeper root, readable by a debug start of ANY profile
    // of it: removed here (a list under another run's live keeper lock is left to that run).
    let mut status = 0;
    match remove_leftover_list(dir) {
        Ok(words) => out.extend(words),
        Err(_) => {
            out.push(
                "list:        a list of test IDs stands in the keeper root and could not be removed: remove it by hand before any other start"
                    .to_owned(),
            );
            status = 1;
        }
    }
    match switch_off(dir) {
        Ok(true) => out.push(format!(
            "switch:      {NETWORK_NAME} switched off in the profile (the login stays){}",
            if alive {
                ""
            } else {
                ": the last run did not end cleanly"
            }
        )),
        Ok(false) => out.push(format!("switch:      {NETWORK_NAME} is off in the profile")),
        Err(_) => {
            out.push(
                "switch:      cannot write the profile's kourier.toml: Threema may stay switched ON; fix the file and run `threema-run stop` again"
                    .to_owned(),
            );
            return Ok((out, 1));
        }
    }
    out.push("app:         not running".to_owned());
    Ok((out, status))
}

/// `stop` as the command line runs it.
pub(super) fn stop_command(a: &Args) -> Result<i32> {
    let dir = a.place.resolve(&var, &target_dir())?;
    let socket = socket_for_client(&dir, &var).ok();
    let term = |pid: u32| {
        if let Ok(pid) = libc::pid_t::try_from(pid) {
            // SAFETY: `kill` with a pid and SIGTERM only sends a signal; the pid is the one the
            // supervisor recorded and is above 1.
            unsafe {
                libc::kill(pid, libc::SIGTERM);
            }
        }
    };
    let (lines, status) = stop_in(&dir, a.force, socket.as_deref(), &StopTiming::REAL, &term)?;
    for line in lines {
        println!("{line}");
    }
    Ok(status)
}
