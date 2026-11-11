//! The steps, part one (T6; `briefs/tr.md` §2.11 and §5, the critic's M1, M5 and R-H2, `wa_run.rs`'s
//! steps): `link`, `sync`, `receive`, `group-receive`, `mirror`, `reconnect` and `passive`, the
//! step runner [`run_step`] that writes each call down, and the client every step reads the app
//! through (`call_at` on the supervisor's control socket).
//!
//! A step is a client of the running supervisor: it watches the app's events and contract lines
//! ([`Client::events`], [`Client::lines`]), reads the app through `state` and `chats` (the only
//! commands a run forwards for a step), asks him ONE question at a time (`ask`, then `answer_of`:
//! his y or n is typed in his own terminal with `threema-run answer`, never here), and tells the
//! judge what he said ([`Client::said`]: S7 `lost` / `twice`, S9 `phone-warning`, S12 `stopped`).
//! A step never sends anything from Kourier, never opens a chat (opening a chat with unread
//! messages marks it read, which writes a receipt), never starts or closes a login except the ONE
//! page change the supervisor offers for his "no" to "is it the TEST phone" ([`Client::close_login`]).
//!
//! Every call of `step` waits at most [`StepTiming::call_max`] (540 s), prints what the user must
//! do (`WINDOW:`, `PHONE:`, `TERMINAL:`, `WAIT:`), prints counts, words and times only (never an ID,
//! a name or a text: a note goes through the redaction), and writes one record to `steps.jsonl`
//! (name, PASS or FAIL, reason, times, notes, what the next call resumes from, facts). A step that
//! runs out of time ends FAIL with a `resume` the next call goes on from, as wa-run's. An unknown
//! step name is never echoed back (the parser does not stop a lower-case ID typed as a step name).
//!
//! THE STOP RULES a step meets: the judge halts the run by itself on what the app prints (S1-S6,
//! S8, S10, S11); a step reads the halt, says what it means in words (`halt_words`: S1a nothing is
//! registered, S1b a device MAY be registered: his phone's linked devices list, then `answer`), and
//! never goes on. The steps add what only they know: S7 (a text lost after 10 minutes, or shown
//! twice), S9 (a phone's warning) and S12 (the emoji differ: he stops).

use std::io::Write as _;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

use super::judge::Said;
use super::launch::socket_for_client;
use super::lines::{Body, Count, Parsed, parse};
use super::profile::{ProfileDir, State, append_line};
use super::prompt::{LINKING_KIND, QUESTIONS, question_drop, question_kind};
use super::supervise::{Refused, call_at, redact, word};
use super::tphone::{self, Origin};
use super::{Args, var};
use crate::target_dir;
use crate::test_phone::Phone;

// ---- Bounds ---------------------------------------------------------------------------------------------

/// The longest one call of `step` waits (well under the ten minutes one shell call may last; a step
/// that needs longer ends FAIL with a `resume` and is run again).
pub(super) const STEP_WAIT_MAX_SECS: u64 = 540;

/// How long the steps wait and how often they look. The tests use short ones.
#[derive(Clone, Copy, Debug)]
pub(super) struct StepTiming {
    /// Between two looks at the supervisor.
    pub poll: Duration,
    /// The longest one call lasts (every wait of a step is cut to what is left of it).
    pub call_max: Duration,
    /// For the app's dev channel to answer at all.
    pub ready_wait: Duration,
    /// From the code shown until a phone started the join (nobody scanned): the link page is closed.
    pub qr_wait: Duration,
    /// From `done` (or a start) to the account being online.
    pub online_wait: Duration,
    /// For one of his answers.
    pub answer_wait: Duration,
    /// A list of chats that stays the same this long is a baseline; a message that arrived is looked
    /// at again after this long (a duplicate would follow at once).
    pub settle: Duration,
    /// The longest the baseline waits for a still list.
    pub baseline_wait: Duration,
    /// After which a text that never arrived is counted lost (S7: 10 minutes from his "sent").
    pub lost_after: Duration,
    /// From his "the personal phone opened it" to the own message reading `read`.
    pub read_wait: Duration,
    /// For the first contract count line (D3: every 60 s while a socket stands).
    pub count_wait: Duration,
    /// For RP-b's drop outcome after a link that failed after its hello (the end waits 12 s).
    pub drop_look: Duration,
    /// Between two looks of the step that watches (`passive`).
    pub look: Duration,
    /// `phone-sleep`: from his "locked" to the personal phone's text (minute 5).
    pub sleep_text_at: Duration,
    /// `phone-sleep`: from his "locked" to the unlock (minute 10).
    pub sleep_total: Duration,
    /// `flight-mode`: from his "flight mode on" to the promotion (D1); none in this time is a FAIL.
    pub d1_wait: Duration,
    /// `flight-mode`: from his "flight mode off" to the personal phone's later text (2 minutes).
    pub later_text_after: Duration,
    /// `removed-on-phone`, `unlink`: from the ask to the close (4113) or the drop's ack.
    pub removal_wait: Duration,
}

impl StepTiming {
    pub(super) const REAL: Self = Self {
        poll: Duration::from_millis(500),
        call_max: Duration::from_secs(STEP_WAIT_MAX_SECS),
        ready_wait: Duration::from_secs(120),
        qr_wait: Duration::from_secs(300),
        online_wait: Duration::from_secs(120),
        answer_wait: Duration::from_secs(300),
        settle: Duration::from_secs(4),
        baseline_wait: Duration::from_secs(90),
        lost_after: Duration::from_secs(600),
        read_wait: Duration::from_secs(300),
        count_wait: Duration::from_secs(90),
        drop_look: Duration::from_secs(20),
        look: Duration::from_secs(5),
        sleep_text_at: Duration::from_secs(300),
        sleep_total: Duration::from_secs(600),
        d1_wait: Duration::from_secs(600),
        later_text_after: Duration::from_secs(120),
        removal_wait: Duration::from_secs(300),
    };

    /// A rehearsal's: the fake answers at once and nobody is slow, so every wait is short (the walk of
    /// `rehearse.rs` runs the steps with these; `step_command` takes them in a rehearsal's place only).
    /// What the fake never prints (a count line, a leader) is waited for only a moment: the step then
    /// says "not measured".
    pub(super) const REHEARSAL: Self = Self {
        poll: Duration::from_millis(100),
        call_max: Duration::from_secs(240),
        ready_wait: Duration::from_secs(60),
        qr_wait: Duration::from_secs(60),
        online_wait: Duration::from_secs(60),
        answer_wait: Duration::from_secs(60),
        settle: Duration::from_secs(1),
        baseline_wait: Duration::from_secs(30),
        lost_after: Duration::from_secs(30),
        read_wait: Duration::from_secs(10),
        count_wait: Duration::from_secs(3),
        drop_look: Duration::from_secs(3),
        look: Duration::from_millis(500),
        sleep_text_at: Duration::from_secs(10),
        sleep_total: Duration::from_secs(20),
        d1_wait: Duration::from_secs(10),
        later_text_after: Duration::from_secs(5),
        removal_wait: Duration::from_secs(10),
    };
}

/// What a step says when the ID was not typed yet.
const ID_NOT_TYPED: &str = "the ID is not typed yet: this start waits for it. In YOUR terminal run `target/debug/xtask threema-run id --profile <name>` and type the TEST ID twice (nothing is shown); then run this step again";
/// What a step says when the start was not for a link.
const NOT_FOR_LINK: &str = "this start is not for a link (it was started without --for-link, or its wait for the ID ended): `threema-run stop`, then `start --confirmed-by-user --for-link` after his go";
/// What a step says while the question of a failed link stands in the keeper root.
const QUESTION_STANDS: &str = "the question of a failed link stands in the keeper root (a device MAY be registered on the TEST phone): answer it first, in YOUR terminal, with `target/debug/xtask threema-run answer --profile <name>`. Nothing was started and nothing was asked";

// ---- The client -----------------------------------------------------------------------------------------

/// What one contract line of the app is, as the supervisor kept and served it (redacted).
#[derive(Clone, Debug)]
pub(super) struct LineSeen {
    pub at_ms: u64,
    pub parsed: Parsed,
}

/// The supervisor did not answer: its socket is gone or silent (a step that was watching a login
/// at `linking` says what that may mean).
#[derive(Debug)]
struct Gone(String);

impl std::fmt::Display for Gone {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for Gone {}

/// The supervisor's control socket, as a step uses it.
#[derive(Clone, Debug)]
pub(super) struct Client {
    socket: PathBuf,
}

impl Client {
    pub(super) fn at(socket: PathBuf) -> Self {
        Self { socket }
    }

    fn call(&self, request: &Value, timeout: Duration) -> Result<Value> {
        call_at(&self.socket, request, timeout).map_err(|e| {
            // A refusal is the supervisor's own answer; anything else is the socket: it is gone.
            if e.downcast_ref::<Refused>().is_some() {
                e
            } else {
                anyhow::Error::new(Gone(format!("{e:#}")))
            }
        })
    }

    pub(super) fn info(&self) -> Result<Value> {
        self.call(&json!({"op": "info"}), Duration::from_secs(10))
    }

    /// The events from sequence `since` on, and the next sequence.
    pub(super) fn events(&self, since: u64) -> Result<(Vec<Value>, u64)> {
        let answer = self.call(
            &json!({"op": "events", "since": since}),
            Duration::from_secs(10),
        )?;
        Ok((
            answer["events"].as_array().cloned().unwrap_or_default(),
            answer["next_seq"].as_u64().unwrap_or(since),
        ))
    }

    /// The contract lines from sequence `since` on, parsed (the served text is redacted already), and
    /// the next sequence.
    pub(super) fn lines(&self, since: u64) -> Result<(Vec<LineSeen>, u64)> {
        let answer = self.call(
            &json!({"op": "lines", "since": since}),
            Duration::from_secs(10),
        )?;
        let lines = answer["lines"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|l| {
                // A served line has its place in the ring (`seq`); one without is not the supervisor's.
                l["seq"].as_u64()?;
                Some(LineSeen {
                    at_ms: l["at_ms"].as_u64().unwrap_or(0),
                    parsed: parse(l["line"].as_str()?)?,
                })
            })
            .collect();
        Ok((lines, answer["next_seq"].as_u64().unwrap_or(since)))
    }

    /// One command of the app's dev channel that a run forwards for a step (`state`, `chats`): its
    /// result, redacted by the supervisor.
    pub(super) fn ctl(&self, cmd: &str, args: Value, timeout: Duration) -> Result<Value> {
        let answer = self.call(
            &json!({"op": "ctl", "cmd": cmd, "args": args, "timeout_ms": timeout.as_millis() as u64}),
            timeout + Duration::from_secs(5),
        )?;
        Ok(answer["result"].clone())
    }

    pub(super) fn state(&self) -> Result<Value> {
        self.ctl("state", json!({}), Duration::from_secs(30))
    }

    pub(super) fn chats(&self) -> Result<Vec<Chat>> {
        Ok(parse_chats(&self.ctl(
            "chats",
            json!({}),
            Duration::from_secs(30),
        )?))
    }

    /// Puts a question ([`QUESTIONS`], by name) to him: it stays open until he answers.
    pub(super) fn ask(&self, name: &str) -> Result<()> {
        self.call(&json!({"op": "ask", "name": name}), Duration::from_secs(10))
            .map(|_| ())
    }

    /// His latest answer to the question `name`: `None` while he has not answered.
    pub(super) fn answer_of(&self, name: &str) -> Result<Option<bool>> {
        let answer = self.call(
            &json!({"op": "answer_of", "name": name}),
            Duration::from_secs(10),
        )?;
        Ok(answer["answer"].as_bool())
    }

    /// [`Client::answer_of`] with where the answer came from (typed in his terminal or relayed by the
    /// lead).
    pub(super) fn answer_with_origin(&self, name: &str) -> Result<Option<(bool, Origin)>> {
        let answer = self.call(
            &json!({"op": "answer_of", "name": name}),
            Duration::from_secs(10),
        )?;
        Ok(answer["answer"].as_bool().map(|yes| {
            (
                yes,
                Origin::from_key(answer["origin"].as_str().unwrap_or("typed")),
            )
        }))
    }

    /// `test-phone-in-hand` (B1, L4): the SUPERVISOR reads the TEST phone's My ID itself and compares
    /// its digest with the one it holds; only a yes or no comes back. `None` when it holds no digest
    /// (an earlier start of the profile took the ID).
    pub(super) fn hand(&self) -> Result<Option<bool>> {
        let answer = self.call(&json!({"op": "hand"}), Duration::from_secs(60))?;
        Ok((answer["known"] == json!(true)).then(|| answer["same"] == json!(true)))
    }

    /// Tells the judge what he said or what the step saw (S7, S9, S12). The kind word of the halt it
    /// raised, if one.
    pub(super) fn said(&self, what: Said) -> Result<Option<String>> {
        let answer = self.call(
            &json!({"op": "said", "what": what.word()}),
            Duration::from_secs(10),
        )?;
        Ok(answer["halted"].as_str().map(str::to_owned))
    }

    /// Tells the judge the user asked for a removal (`unlink`, `removed-on-phone`): 4113,
    /// `logged_out` and the drop frame are the removal's own from now on (R-L2). Set BEFORE he is
    /// told to act. The supervisor refuses it in a halted run.
    pub(super) fn removal(&self, what: &str) -> Result<()> {
        self.call(
            &json!({"op": "removal", "what": what}),
            Duration::from_secs(10),
        )
        .map(|_| ())
    }

    /// Gives the removal back to the judge (LOW-9): an end step that failed no longer excuses a 4113,
    /// a `logged_out` or a drop frame.
    pub(super) fn removal_over(&self, what: &str) -> Result<()> {
        self.call(
            &json!({"op": "removal_over", "what": what}),
            Duration::from_secs(10),
        )
        .map(|_| ())
    }

    /// Opens a send step's window: until [`Self::send_window_over`] the judge lets the frames of the
    /// user's own send through, within its bounds. Set BEFORE they are told to send. Idempotent.
    pub(super) fn send_window(&self, what: &str) -> Result<()> {
        self.call(
            &json!({"op": "send_window", "what": what}),
            Duration::from_secs(10),
        )
        .map(|_| ())
    }

    /// A send step's own send (`--lead-sends`): the supervisor opens the chat, attaches the tool's
    /// picture when `picture`, and sends `text`.
    pub(super) fn step_send(
        &self,
        what: &str,
        conversation: i64,
        text: &str,
        picture: bool,
    ) -> Result<()> {
        self.call(
            &json!({"op": "step_send", "what": what, "conversation": conversation, "text": text, "picture": picture}),
            crate::threema_run::supervise::STEP_SEND_WITHIN,
        )
        .map(|_| ())
    }

    /// Closes a send step's window: what it wrote stays allowed, nothing further is.
    pub(super) fn send_window_over(&self, what: &str) -> Result<()> {
        self.call(
            &json!({"op": "send_window_over", "what": what}),
            Duration::from_secs(10),
        )
        .map(|_| ())
    }

    /// Asks the supervisor to leave the link page for the list: `Ok(true)` only when the app took it,
    /// `Ok(false)` when no login waits at its code or emoji (nothing to close) or the app refused.
    pub(super) fn close_login(&self) -> Result<bool> {
        match self.call(&json!({"op": "close_login"}), Duration::from_secs(30)) {
            Ok(answer) => Ok(answer["closed"] == json!(true)),
            Err(e) => match e.downcast_ref::<Refused>() {
                Some(refused) if refused.code == "notWaiting" => Ok(false),
                _ => Err(e),
            },
        }
    }
}

/// Calls `f` every `every` until it returns a value or `timeout` passes.
fn poll<T>(
    timeout: Duration,
    every: Duration,
    mut f: impl FnMut() -> Result<Option<T>>,
) -> Result<Option<T>> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(v) = f()? {
            return Ok(Some(v));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        std::thread::sleep(every);
    }
}

// ---- What the app shows ----------------------------------------------------------------------------------

/// One row of dev-control `chats`: counts and words only.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Chat {
    pub id: i64,
    pub kind: String,
    pub is_self: bool,
    pub unread: i64,
    /// When the newest thing in the chat happened (ms since the epoch; 0: nothing).
    pub last_activity: i64,
    /// The receipt of the newest own message as the list shows it (`none`, `pending`, `sent`,
    /// `delivered`, `read`, `failed`).
    pub newest_own: String,
}

fn parse_chats(v: &Value) -> Vec<Chat> {
    v["chats"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| Chat {
            id: c["conversation"].as_i64().unwrap_or(0),
            kind: c["kind"].as_str().unwrap_or("other").to_owned(),
            is_self: c["self"] == json!(true),
            unread: c["unread"].as_i64().unwrap_or(0),
            last_activity: c["last_activity"].as_i64().unwrap_or(0),
            newest_own: c["newest_own"].as_str().unwrap_or("none").to_owned(),
        })
        .collect()
}

/// What a step keeps of one chat for a later step to compare (`facts.chats`): the local number of
/// the conversation (not an ID) and two counts.
pub(super) fn chat_fact(c: &Chat) -> Value {
    json!({"chat": c.id, "unread": c.unread, "last_activity": c.last_activity})
}

/// What the app shows at one moment: the list, the open conversation and its message rows. The
/// step opens no chat; the rows are those of a chat HE opened.
#[derive(Clone, Debug, Default, PartialEq)]
struct Snap {
    chats: Vec<Chat>,
    open: i64,
    rows: Vec<Value>,
}

impl Snap {
    fn take(client: &Client) -> Result<Self> {
        let chats = client.chats()?;
        let state = client.state()?;
        Ok(Self {
            chats,
            open: state["open"].as_i64().unwrap_or(0),
            rows: state["rows"].as_array().cloned().unwrap_or_default(),
        })
    }

    /// The part of the list that says something arrived or was read.
    fn list(&self) -> Vec<(i64, i64, i64)> {
        self.chats
            .iter()
            .map(|c| (c.id, c.unread, c.last_activity))
            .collect()
    }

    fn to_value(&self) -> Value {
        json!({
            "chats": self.chats.iter().map(|c| json!({
                "id": c.id, "kind": c.kind, "self": c.is_self, "unread": c.unread,
                "last_activity": c.last_activity, "newest_own": c.newest_own,
            })).collect::<Vec<_>>(),
            "open": self.open,
            "newest": newest_message(&self.rows),
        })
    }

    /// A baseline a step wrote into its `resume` (only the newest row number is kept of the rows).
    fn from_value(v: &Value) -> Option<(Self, i64)> {
        let chats = v["chats"]
            .as_array()?
            .iter()
            .map(|c| Chat {
                id: c["id"].as_i64().unwrap_or(0),
                kind: c["kind"].as_str().unwrap_or("other").to_owned(),
                is_self: c["self"] == json!(true),
                unread: c["unread"].as_i64().unwrap_or(0),
                last_activity: c["last_activity"].as_i64().unwrap_or(0),
                newest_own: c["newest_own"].as_str().unwrap_or("none").to_owned(),
            })
            .collect();
        Some((
            Self {
                chats,
                open: v["open"].as_i64().unwrap_or(0),
                rows: Vec::new(),
            },
            v["newest"].as_i64().unwrap_or(0),
        ))
    }
}

/// The highest message id among `rows`.
fn newest_message(rows: &[Value]) -> i64 {
    rows.iter()
        .filter_map(|r| r["message"].as_i64())
        .max()
        .unwrap_or(0)
}

/// What came in since a baseline, for the chats of one kind.
#[derive(Debug, Default, PartialEq, Eq)]
struct Arrivals {
    /// Chats the baseline knew: (conversation, messages from others that are new).
    known: Vec<(i64, i64)>,
    /// Chats the baseline did NOT have, with unread messages in them: Kourier starts no chat.
    fresh: Vec<i64>,
}

impl Arrivals {
    fn none(&self) -> bool {
        self.known.is_empty() && self.fresh.is_empty()
    }
}

/// How many messages from others arrived per chat of `kind` since the baseline: in the chat that is
/// open then and now, the rows newer than the baseline's newest one (an open chat is read at once,
/// so its unread count stays 0); in every other chat, the growth of its unread count.
fn arrivals(
    base_chats: &[Chat],
    base_open: i64,
    base_newest: i64,
    now: &Snap,
    kind: &str,
) -> Arrivals {
    let mut found = Arrivals::default();
    for c in now.chats.iter().filter(|c| c.kind == kind && !c.is_self) {
        let Some(before) = base_chats.iter().find(|b| b.id == c.id) else {
            if c.unread > 0 {
                found.fresh.push(c.id);
            }
            continue;
        };
        let count = if c.id == now.open && c.id == base_open {
            let new = now
                .rows
                .iter()
                .filter(|r| r["own"] != json!(true))
                .filter(|r| r["message"].as_i64().is_some_and(|m| m > base_newest))
                .count();
            i64::try_from(new).unwrap_or(i64::MAX)
        } else {
            c.unread - before.unread
        };
        if count > 0 {
            found.known.push((c.id, count));
        }
    }
    found
}

// ---- The words of a halt ---------------------------------------------------------------------------------

/// What a halt means for the user, by its kind word (the judge's: `judge.rs` lists them). S1a: nothing
/// is registered. S1b: a device MAY be registered, so his phone's linked devices list comes before
/// anything else and `answer` asks it once the run has ended.
fn halt_words(kind: &str) -> String {
    let kind = word(kind);
    match kind.as_str() {
        "link-refused" | "list-not-taken" => format!(
            "the run is halted ({kind}): the pairing was refused before Kourier stored or registered anything, so nothing is registered on the TEST phone. Do NOT scan any code. A new link is another start with --after-halt and his word"
        ),
        "link-failed-after-hello" => format!(
            "the run is halted ({kind}): the login failed after the phone confirmed it, so a device MAY be registered on the TEST phone. On the TEST phone open Threema's linked devices (the personal phone never opens that list) and remove any Kourier entry. When the run has ended, `threema-run answer` in YOUR terminal asks you about it; no new link before that"
        ),
        "stopped" => format!(
            "the run is halted ({kind}): he said stop. Nothing is retried; a new start needs --after-halt and his word"
        ),
        _ => format!(
            "the run is halted ({kind}): nothing is retried. Tell the user the kind and read threema-run/app.log; a new start needs --after-halt and his word"
        ),
    }
}

/// The sentence of Kourier's own drop of the device after a link that failed after its hello (S1b, RP-b): `drop` is
/// the outcome's word (`removed`, `not-reached`; anything else or none: not confirmed).
fn drop_words(drop: Option<&str>) -> &'static str {
    match drop {
        Some("removed") => {
            ". Kourier's own drop of the device: the mediator said Removed (check the list anyway)"
        }
        Some("not-reached") => {
            ". Kourier's own drop of the device did NOT reach the mediator: the entry is probably still listed"
        }
        _ => ". Kourier's own drop of the device was not confirmed",
    }
}

/// RT1 attempt 1: the drop's outcome a supervisor that is gone left in the profile: the halt's record in the state
/// file (`halted.drop`, written by its `note_drop` once the outcome line came or its wait ended), else the keeper
/// root's open question (`drop`, written at the app's exit). `None` while neither names one (the end may still be
/// writing them).
fn saved_drop(dir: &ProfileDir) -> Option<String> {
    State::load(dir)
        .ok()
        .flatten()
        .and_then(|s| s.get("halted")["drop"].as_str().map(str::to_owned))
        .or_else(|| question_drop(dir))
}

// ---- One running step ------------------------------------------------------------------------------------

/// One running step: what it may say to the user, what it noted, and the profile it acts on.
struct StepRun<'a> {
    client: &'a Client,
    dir: &'a ProfileDir,
    args: &'a Args,
    timing: StepTiming,
    /// Called with every line said to the user (the tests wait on it; the real run passes nothing).
    heard: &'a dyn Fn(&str),
    notes: Vec<String>,
    /// Every line said to the user, in order.
    said: Vec<String>,
    /// What the latest unfinished call of this step left for this one (its `resume` record).
    previous: Option<Value>,
    /// What this call leaves for the next one when it runs out of time.
    resume: Option<Value>,
    /// Structured facts a later step reads from the step log (`facts.chats`).
    facts: Option<Value>,
    /// What the steps of part two (`part2`) carry from one call to the next: the phases done, the
    /// baselines, the answers given. It is the `resume` of a call that ends FAIL ([`StepRun::pause`]).
    carry: serde_json::Map<String, Value>,
    /// What a step of part two saw, in counts, bools and words, for the report (`facts.obs`); it is
    /// carried with a resume, so the call that ends the step has what the earlier calls saw.
    obs: serde_json::Map<String, Value>,
    started: Instant,
    /// The step's name (the words of "run `step <name>` again").
    step: &'static str,
    /// The TEST phone of a run started with `--test-phone` (D1); `None` otherwise, and then every
    /// question goes to the user as before.
    tp: Option<Box<dyn Phone>>,
    /// Where each answer of this call came from (`facts.origins`).
    origins: Vec<Value>,
}

impl StepRun<'_> {
    fn say(&mut self, line: String) {
        println!("{line}");
        let _ = std::io::stdout().flush();
        (self.heard)(&line);
        self.said.push(line);
    }

    fn note(&mut self, text: impl Into<String>) {
        self.notes.push(text.into());
    }

    /// What the user does on a phone.
    fn phone(&mut self, what: &str) {
        self.say(format!("PHONE: {what}"));
    }

    /// What the user does in the Kourier window.
    fn window(&mut self, what: &str) {
        self.say(format!("WINDOW: {what}"));
    }

    /// The step now waits for the user.
    fn waiting(&mut self, what: &str) {
        self.say(format!("WAIT: {what}"));
    }

    /// What the user types in his own terminal.
    fn terminal(&mut self, what: &str) {
        self.say(format!("TERMINAL: {what}"));
    }

    /// What is left of this call.
    fn left(&self) -> Duration {
        self.timing.call_max.saturating_sub(self.started.elapsed())
    }

    /// `--seconds` or `default` seconds, never more than one call may last.
    fn wait_for(&self, default: u64) -> Duration {
        Duration::from_secs(self.args.seconds.unwrap_or(default).min(STEP_WAIT_MAX_SECS))
            .min(self.timing.call_max)
    }

    /// The run is halted: no step goes on. The words say what the halt means.
    fn halted_now(&self) -> Result<()> {
        if let Some(kind) = self.client.info()?["halted"].as_str() {
            return Err(self.halt_error(kind));
        }
        Ok(())
    }

    /// What a halt says, with Kourier's own drop of the device when the join failed after its hello
    /// (S1b): the outcome line is waited for a moment (the supervisor's end waits for it too). A
    /// supervisor that ended meanwhile (RT1 attempt 1: its end waited for the line, noted it and went
    /// before the next look) left the outcome in the profile: [`saved_drop`].
    fn halt_error(&self, kind: &str) -> anyhow::Error {
        let mut words = halt_words(kind);
        if kind == "link-failed-after-hello" {
            let drop = poll(self.timing.drop_look, self.timing.poll, || {
                match self.client.info() {
                    Ok(info) => Ok(info["drop"].as_str().map(str::to_owned)),
                    // The supervisor ended: what its end saved.
                    Err(_) => Ok(saved_drop(self.dir)),
                }
            })
            .ok()
            .flatten();
            words.push_str(drop_words(drop.as_deref()));
        }
        anyhow!("{words}")
    }

    /// His answer to the question `name`, asked now: `None` when he did not answer in time. A yes of
    /// an earlier call of this run is reused (`reuse_no` also reuses a no); the question is asked
    /// fresh otherwise. The text he sees is [`QUESTIONS`]'s, in his own terminal.
    fn ask_him(&mut self, name: &str, reuse_no: bool) -> Result<Option<bool>> {
        // A run with a test phone answers by a check where the question has one.
        if let Some(answer) = self.auto_answer(name)? {
            return Ok(Some(answer));
        }
        if let Some(earlier) = self.client.answer_of(name)?
            && (earlier || reuse_no)
        {
            if self.has_phone()
                && let Some((_, origin)) = self.client.answer_with_origin(name)?
                && !self.origins.iter().any(|o| o["question"] == json!(name))
            {
                self.record_origin(name, origin);
            }
            return Ok(Some(earlier));
        }
        self.client.ask(name)?;
        self.tell_question(name);
        let bound = self.left().min(self.timing.answer_wait);
        if self.has_phone() {
            return self.person_answer(name, bound);
        }
        let answer = poll(bound, self.timing.poll, || {
            self.halted_now()?;
            self.client.answer_of(name)
        })?;
        Ok(answer)
    }

    /// Says where the question is answered.
    fn tell_question(&mut self, name: &str) {
        let text = QUESTIONS
            .iter()
            .find(|(q, _)| *q == name)
            .map_or("a question", |(_, text)| *text);
        // The emoji comparison is HIS, typed in his terminal, never relayed (H5, M7).
        if self.has_phone() && name != EMOJI {
            let text = if name == "tp-list-clean" {
                "On the TEST phone, does Threema 2.0 for Desktop list no device at all?"
            } else if name == "phone-warning" {
                "The TEST phone's screen and shade were checked by the tool. Does the PERSONAL phone show a warning about an unofficial client, a security warning about the device, or a request to verify the account? (A line about forward security in a chat is none of these)"
            } else {
                text
            };
            self.ask_user_lines(name, text);
            return;
        }
        self.terminal(&format!(
            "in YOUR terminal run `target/debug/xtask threema-run answer --profile {}` and press y or n: {text}",
            self.dir.label()
        ));
    }

    /// Asks the S9 question after a phone question (every phone question ends with it): a yes halts
    /// the run through the judge (`phone-warning`).
    fn ask_the_warning(&mut self) -> Result<()> {
        match self.ask_him("phone-warning", true)? {
            Some(true) => {
                let _ = self.client.said(Said::PhoneWarning)?;
                bail!(
                    "a phone shows a warning (an unofficial client, the device, or a request to verify the account): the run is halted (phone-warning). Do not touch the warning on the phone; tell the user. A new start needs --after-halt and his word"
                )
            }
            Some(false) => Ok(()),
            None => bail!(
                "no answer to the warning question in time: run this step again, it asks what is open"
            ),
        }
    }

    // ---- What the steps of part two carry between calls ------------------------------------------------

    /// This call ends FAIL and the next one goes on from what the step carries (`resume`): the
    /// phases done, the baselines, his answers, and what the report will read (`obs`).
    fn pause(&mut self, why: impl Into<String>) -> anyhow::Error {
        let mut resume = self.carry.clone();
        resume.insert("obs".to_owned(), Value::Object(self.obs.clone()));
        self.resume = Some(Value::Object(resume));
        anyhow!("{}", why.into())
    }

    fn carried_u64(&self, key: &str) -> Option<u64> {
        self.carry.get(key).and_then(Value::as_u64)
    }

    /// The answer to `name` an earlier call of this step got ([`StepRun::asked_once`]).
    fn carried_answer(&self, name: &str) -> Option<bool> {
        self.carry
            .get("answers")
            .and_then(|a| a.get(name))
            .and_then(Value::as_bool)
    }

    fn carried_bool(&self, key: &str) -> bool {
        self.carry.get(key).and_then(Value::as_bool) == Some(true)
    }

    fn carry_set(&mut self, key: &str, value: Value) {
        self.carry.insert(key.to_owned(), value);
    }

    /// What the report will read: counts, bools and words, never a value.
    fn see(&mut self, key: &str, value: Value) {
        self.obs.insert(key.to_owned(), value);
    }

    /// His answer to the question `name`, asked once in the whole step: an answer an earlier call
    /// of this step got is carried, a missing one ends this call ([`StepRun::pause`]) with the
    /// question still open.
    ///
    /// `reuse_no`: a "no" he gave between two calls stands too (else a no is asked again).
    fn asked_once(&mut self, name: &str, reuse_no: bool) -> Result<bool> {
        if let Some(answer) = self.carried_answer(name) {
            return Ok(answer);
        }
        let asked = match self.ask_him(name, reuse_no) {
            Ok(a) => a,
            // A screen that could not be read: this call ends, the next goes on from here.
            Err(e) if self.has_phone() && e.to_string().contains("run `step") => {
                return Err(self.pause(format!("{e:#}")));
            }
            Err(e) => return Err(e),
        };
        let Some(answer) = asked else {
            return Err(self.pause(format!(
                "no answer to the question \"{name}\" in time (it stays open): answer it in your terminal, then run this step again, it goes on from here"
            )));
        };
        let mut answers = self
            .carry
            .get("answers")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        answers.insert(name.to_owned(), json!(answer));
        self.carry_set("answers", Value::Object(answers));
        Ok(answer)
    }

    /// `asked_once` for the S9 question (a): a yes halts the run through the judge.
    fn asked_the_warning_once(&mut self) -> Result<()> {
        if self.asked_once("phone-warning", false)? {
            let _ = self.client.said(Said::PhoneWarning)?;
            bail!(
                "a phone shows a warning (an unofficial client, the device, or a request to verify the account): the run is halted (phone-warning). Do not touch the warning on the phone; tell the user. A new start needs --after-halt and his word"
            )
        }
        Ok(())
    }
}

// ---- Step `link` ---------------------------------------------------------------------------------------------

/// What the watch of a login has seen so far.
#[derive(Default)]
struct LinkWatch {
    since: u64,
    qr_up: bool,
    qr_shown: bool,
    told_scan: bool,
    warned_wait: bool,
    qr_at: Option<Instant>,
    emoji_up: bool,
    emoji_asked: bool,
    past_code: bool,
    done: bool,
    hand: Option<bool>,
}

impl LinkWatch {
    fn take(&mut self, events: &[Value]) {
        for e in events {
            let data = &e["data"];
            match e["event"].as_str() {
                Some("login_qr") if data["shown"] == json!(true) => self.qr_shown = true,
                Some("login_step") => {
                    let (step, kind) = (
                        data["step"].as_str().unwrap_or_default(),
                        data["kind"].as_str().unwrap_or_default(),
                    );
                    match (step, kind) {
                        ("qr", "qr") => self.qr_up = true,
                        ("emoji", "emoji") => {
                            self.emoji_up = true;
                            self.past_code = true;
                        }
                        ("linking", _) => self.past_code = true,
                        (_, "done") => {
                            self.done = true;
                            self.past_code = true;
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
    }
}

/// D2 for `test-phone-in-hand` (B1): the SUPERVISOR reads the TEST phone's My ID at its kept serial
/// and compares its digest with the one it holds; this step learns a yes or a no. Nothing of the ID
/// or of a digest crosses the socket.
fn hand_by_check(run: &mut StepRun<'_>) -> Result<bool> {
    let same = run.client.hand().map_err(|e| {
        if e.downcast_ref::<Refused>().is_some() {
            anyhow!(
                "the TEST phone's My ID could not be read by the supervisor as an 8-character ID: nothing was scanned; run `step link` again"
            )
        } else {
            e
        }
    })?;
    match same {
        Some(same) => {
            run.record_origin(HAND, Origin::TestPhone);
            Ok(same)
        }
        None => bail!(
            "the supervisor holds no digest of the TEST ID in this start: nothing was decided; `stop`, then start again with --for-link and take the ID with `id --from-test-phone`"
        ),
    }
}

const HAND: &str = "test-phone-in-hand";
const EMOJI: &str = "emoji-equal";

/// The words of the link step's "no" (critic M1): nobody scans. `closed`: the page that held a code
/// was left; else no code was up.
fn not_the_test_phone(closed: bool) -> String {
    format!(
        "you said the phone in your hand is NOT the TEST phone: {}, and nothing was scanned. Take the TEST phone (the personal phone never opens Threema's linked devices); do not open Add Account in the window before you hold it; then run `step link` again",
        if closed {
            "the link page was closed so no code stays up"
        } else {
            "no code was up, so there was nothing to close"
        }
    )
}

/// Waits for the login to end, telling him when to scan. The question "is it the TEST phone" is open
/// from the start; "scan now" is said only after his yes and only while a code is up.
fn watch_the_login(run: &mut StepRun<'_>) -> Result<()> {
    let mut w = LinkWatch::default();
    // A run with a test phone checked the phone in hand before the watch (`hand_by_check`).
    if let Some(h) = run.carry.get("hand").and_then(Value::as_bool) {
        w.hand = Some(h);
    }
    loop {
        match watch_once(run, &mut w) {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            // The supervisor is gone. If the login stood at `linking` the app died or was killed
            // there and the join may have registered a device (S1b): what the step saw, or the mark
            // the supervisor kept durably before it passed the `linking` step on (the keeper root's
            // file, `link_at` in the state), says so.
            Err(e)
                if e.downcast_ref::<Gone>().is_some()
                    && ((w.past_code && !w.done)
                        || question_kind(run.dir).is_some()
                        || State::load(run.dir)
                            .ok()
                            .flatten()
                            .is_some_and(|s| s.link_stood())) =>
            {
                bail!(
                    "{} (the supervisor and the app are gone while the login stood at linking){}",
                    halt_words("link-failed-after-hello"),
                    drop_words(saved_drop(run.dir).as_deref())
                );
            }
            Err(e) => return Err(e),
        }
        std::thread::sleep(run.timing.poll);
    }
}

/// One look at the login: `Ok(true)` when it is done.
fn watch_once(run: &mut StepRun<'_>, w: &mut LinkWatch) -> Result<bool> {
    let info = run.client.info()?;
    if let Some(kind) = info["halted"].as_str() {
        return Err(run.halt_error(kind));
    }
    let (events, next) = run.client.events(w.since)?;
    w.since = next;
    w.take(&events);
    if w.hand.is_none() {
        w.hand = run.client.answer_of(HAND)?;
    }
    if w.hand == Some(false) {
        // Nobody may scan: leave the page that holds the code (not a halt).
        let closed = run.client.close_login()?;
        run.note(if closed {
            "the link page was closed on his no: no code stays up"
        } else {
            "his no came while no code waited: nothing to close"
        });
        bail!("{}", not_the_test_phone(closed));
    }
    if w.qr_up && w.qr_shown && !w.past_code && !w.told_scan {
        if w.hand == Some(true) {
            run.personal(
                    "scan now. On the TEST phone (never the personal one): Threema, the multi-device menu, add a device, and scan the QR code in the Kourier window.",
                );
            w.told_scan = true;
            w.qr_at = Some(Instant::now());
        } else if !w.warned_wait {
            run.say(
                    "WAIT: a code is up. Do NOT scan it until you answered \"is it the TEST phone\" with y (terminal, `threema-run answer`)".to_owned(),
                );
            w.warned_wait = true;
        }
    }
    if w.emoji_up && !w.emoji_asked {
        run.personal(
                "compare the three emoji on the Kourier window and on the TEST phone; confirm ON THE PHONE only if they are equal; then answer here",
            );
        run.client.ask(EMOJI)?;
        run.tell_question(EMOJI);
        w.emoji_asked = true;
    }
    if w.emoji_asked && !w.done {
        let got = if run.has_phone() {
            run.client.answer_with_origin(EMOJI)?
        } else {
            run.client.answer_of(EMOJI)?.map(|y| (y, Origin::Typed))
        };
        if let Some((_, origin)) = got
            && run.has_phone()
            && !run.origins.iter().any(|o| o["question"] == json!(EMOJI))
        {
            run.record_origin(EMOJI, origin);
        }
        match got.map(|(yes, _)| yes) {
            Some(false) => {
                // He says they differ: cancel on the phone; the run stops (S2, S12).
                let _ = run.client.said(Said::Stop)?;
                bail!(
                    "you said the emoji differ: CANCEL on the TEST phone now, confirm nothing. The run is halted (stopped); no second link in this run without his word"
                );
            }
            Some(true) if !run.notes.iter().any(|n| n.starts_with("emoji:")) => {
                run.note("emoji: he said the three were equal");
            }
            _ => {}
        }
    }
    if w.done {
        return Ok(true);
    }
    if let Some(at) = w.qr_at
        && !w.past_code
        && at.elapsed() >= run.timing.qr_wait
    {
        let closed = run.client.close_login()?;
        bail!(
            "no phone started the join in {} s; {}. Look at the TEST phone's linked devices before anything else; a new attempt needs a new `step link`",
            run.timing.qr_wait.as_secs(),
            if closed {
                "the link page was closed so no code stays up"
            } else {
                "the link page could NOT be confirmed closed: `threema-run stop` now"
            }
        );
    }
    if run.left().is_zero() {
        bail!(
            "the step's time ran out before the login ended: run `step link` again, it picks the login up where it stands. Do not stop the app while a link may be in flight"
        );
    }
    Ok(false)
}

/// Step `link`: the first link of a profile by the QR code, in a start made for it. His click in the
/// window starts the login (the tool opens no login), his scan on the TEST phone joins it.
fn step_link(run: &mut StepRun<'_>) -> Result<()> {
    let info = run.client.info()?;
    if let Some(kind) = info["halted"].as_str() {
        return Err(run.halt_error(kind));
    }
    // The keeper root's `linking` mark of THIS run's own login is no question (T4): only a question
    // of an earlier link, or a mark nobody's login owns, stops the step.
    let own_linking =
        question_kind(run.dir).as_deref() == Some(LINKING_KIND) && info["link_at"] == json!(true);
    if question_kind(run.dir).is_some() && !own_linking {
        bail!("{QUESTION_STANDS}");
    }
    match info["id"].as_str() {
        Some("taken") => {}
        Some("waiting") => bail!("{ID_NOT_TYPED}"),
        _ => bail!("{NOT_FOR_LINK}"),
    }
    let login_seen = info["login_seen"] == json!(true);
    let done_here = login_seen && info["login"]["kind"] == json!("done");
    if !login_seen && State::load(run.dir)?.is_some_and(|s| s.linked()) {
        bail!(
            "this profile has an account already: `link` is for a profile without one, and no second link is allowed in it"
        );
    }
    if done_here {
        run.note("the login was done already in this run: only the phone's questions are asked");
    } else {
        if login_seen {
            run.note("a login was already underway: waiting for it, nothing was started again");
        } else {
            run.say(
                "Take the TEST phone for this step; the personal phone stays face down and never opens Threema's linked devices.".to_owned(),
            );
            run.window(
                "in the Kourier window: Add Account, Threema, read the warning, tick the box and press Continue. The code follows.",
            );
        }
        // The question comes first, before any code can be up (critic M1). A login that is past the
        // code (the phone scanned already) needs no scan: only the emoji and the end are watched.
        if !matches!(info["login"]["step"].as_str(), Some("emoji" | "linking")) {
            if run.has_phone() {
                let h = hand_by_check(run)?;
                run.carry_set("hand", json!(h));
            } else {
                run.client.ask(HAND)?;
                run.tell_question(HAND);
            }
        }
        run.waiting(
            "for the warning to be accepted in the window, then for the TEST phone to scan",
        );
        let started = Instant::now();
        watch_the_login(run)?;
        run.note(format!(
            "the login reached done {} s into the step",
            started.elapsed().as_secs()
        ));
    }
    // `done` and the account active.
    let active = poll(
        run.timing.online_wait.min(run.left()),
        run.timing.poll,
        || {
            run.halted_now()?;
            let info = run.client.info()?;
            Ok((info["account"]["state"] == json!("active")).then_some(info))
        },
    )?;
    let Some(info) = active else {
        bail!(
            "the login is done but the account did not become active in {} s (read threema-run/app.log)",
            run.timing.online_wait.as_secs()
        );
    };
    run.note(format!(
        "allow list: {}",
        safe_text(info["allow_list"].as_str().unwrap_or("none"))
    ));
    run.note("the account is active");
    run.facts = Some(json!({"linked": true}));
    // The questions at the end: the phone's list, the menu path, and S9's.
    let list = run.ask_him("list-shows-kourier", true)?;
    let Some(list) = list else {
        bail!(
            "no answer to \"does the TEST phone's list show Kourier\" in time: run `step link` again, it asks what is open"
        );
    };
    run.note(format!(
        "the TEST phone's linked devices list shows the Kourier entry: {}",
        if list { "yes" } else { "NO" }
    ));
    let menu = run.ask_him("menu-path-as-runbook", true)?;
    match menu {
        Some(yes) => run.note(format!(
            "the menu path on the TEST phone was the runbook's: {}",
            if yes {
                "yes"
            } else {
                "NO (the words need a fix)"
            }
        )),
        None => bail!(
            "no answer to \"was the menu path the runbook's\" in time: run `step link` again, it asks what is open"
        ),
    }
    run.ask_the_warning()?;
    if !list {
        bail!(
            "the TEST phone's list does NOT show a Kourier entry although the login is done: a finding (the device's label or the registration); nothing is retried"
        );
    }
    Ok(())
}

// ---- Step `sync` -----------------------------------------------------------------------------------------------

/// Whether a count line says what a FOLLOWER may say: it read no chat server message, wrote no
/// unblock, acknowledgement, inbound reflect, ephemeral, transaction, drop or "other", no `transmit`
/// frame, and held nothing. Jobs' frames pass (R-H2).
fn follower_words_only(c: &Count) -> bool {
    [
        "in_csp_message",
        "out_message_ack",
        "out_unblock",
        "out_reflect_inbound",
        "out_ephemeral",
        "out_tx",
        "out_drop",
        "out_other",
        "out_reflect_transmit",
        "out_box_transmit",
        "held",
    ]
    .iter()
    .all(|f| c.get(f) == 0)
}

/// A count line as a note: the numbers a step reports (no value, no name).
fn counts_note(c: &Count) -> String {
    format!(
        "when={} in_reflected={} in_csp_message={} out_reflected_ack={} out_reflect_job={} out_box_job={} out_unblock={} out_message_ack={} out_idle={} held={}",
        if c.end { "end" } else { "running" },
        c.get("in_reflected"),
        c.get("in_csp_message"),
        c.get("out_reflected_ack"),
        c.get("out_reflect_job"),
        c.get("out_box_job"),
        c.get("out_unblock"),
        c.get("out_message_ack"),
        c.get("out_idle"),
        c.get("held"),
    )
}

/// The count lines among `lines`.
fn counts_of(lines: &[LineSeen]) -> Vec<&Count> {
    lines
        .iter()
        .filter_map(|l| match &l.parsed.body {
            Body::Count(c) => Some(c),
            _ => None,
        })
        .collect()
}

/// Step `sync`: the account goes `syncing`, then `online`; the time from `done`; the chats; no
/// promotion while the phone leads; the first count line holds follower words only.
fn step_sync(run: &mut StepRun<'_>) -> Result<()> {
    run.halted_now()?;
    let linked = State::load(run.dir)?;
    if !linked.as_ref().is_some_and(|s| s.linked()) {
        bail!("this profile is not linked: run `step link` first");
    }
    run.waiting("for the account to be online");
    let online = poll(
        run.timing.online_wait.min(run.left()),
        run.timing.poll,
        || {
            run.halted_now()?;
            let info = run.client.info()?;
            Ok((info["online"] == json!(true)).then_some(info))
        },
    )?;
    let Some(info) = online else {
        bail!(
            "the account is not online after {} s (account: {}): read threema-run/app.log",
            run.timing.online_wait.as_secs(),
            run.client.info()?["account"]
        );
    };
    let (events, _) = run.client.events(0)?;
    let conns: Vec<&str> = events
        .iter()
        .filter(|e| e["event"] == "account_state")
        .filter_map(|e| e["data"]["conn"].as_str())
        .collect();
    run.note(format!(
        "the account went through: {}",
        if conns.is_empty() {
            "(no account_state event)".to_owned()
        } else {
            conns.join(" > ")
        }
    ));
    if let Some(state) = State::load(run.dir)?
        && let (Some(done), Some(up)) = (state.num("linked_since_ms"), state.num("last_online_ms"))
        && info["login_seen"] == json!(true)
        && up >= done
    {
        run.note(format!(
            "online {} s after the login was done",
            (up - done) / 1000
        ));
    }
    let chats = run.client.chats()?;
    run.note(format!(
        "{} direct and {} group conversations listed",
        chats
            .iter()
            .filter(|c| c.kind == "direct" && !c.is_self)
            .count(),
        chats.iter().filter(|c| c.kind == "group").count()
    ));
    // The first count line (running, every 60 s; else the socket's end).
    let first = poll(
        run.timing.count_wait.min(run.left()),
        run.timing.poll,
        || {
            run.halted_now()?;
            let (lines, _) = run.client.lines(0)?;
            Ok((!counts_of(&lines).is_empty()).then_some(lines))
        },
    )?;
    let mut finding = None;
    match first {
        Some(lines) => {
            let counts = counts_of(&lines);
            let c = counts[0];
            run.note(format!("first count line: {}", counts_note(c)));
            if follower_words_only(c) {
                run.note("the count line holds follower words only");
            } else {
                finding = Some(
                    "a count line holds words a follower may not say (the judge halts on the ones that are risks)",
                );
            }
        }
        None => {
            let words = format!(
                "frame counts: not measured (no count line came in {} s; the line comes every 60 s while a socket stands)",
                run.timing.count_wait.as_secs()
            );
            run.note(words);
            // MEDIUM-1: in a real place a run without count lines is unjudged (S5a-d and S6 read
            // nothing): the step FAILS and the supervisor's watchdog halts the run. Only the
            // rehearsal's fake prints none and may pass.
            if !run.dir.rehearsal {
                finding = Some(
                    "no count line came: the count rules (S5a-d, S6) judge nothing. The log filter or the backend's line is wrong: stop the run and read threema-run/app.log",
                );
            }
        }
    }
    let info = run.client.info()?;
    let d1 = info["lines"]["D1_UNBLOCKED"].as_u64().unwrap_or(0);
    if info["leader"] == json!(true) || d1 > 0 {
        finding = Some(
            "Kourier became the leader during sync while the TEST phone should lead (D1 was seen): a finding, not a halt",
        );
    } else {
        run.note("no leader promotion was seen (the phone leads)");
    }
    run.facts = Some(json!({"chats": chats.iter().map(chat_fact).collect::<Vec<_>>()}));
    match finding {
        Some(f) => bail!("{f}"),
        None => Ok(()),
    }
}

// ---- Steps `receive` and `group-receive` ---------------------------------------------------------------

/// A baseline to count arrivals against: the account is online and the list of conversations has
/// stayed the same for `timing.settle`.
fn settled_baseline(run: &mut StepRun<'_>) -> Result<Snap> {
    let deadline = Instant::now() + run.timing.baseline_wait.min(run.left());
    let online = poll(
        run.timing.baseline_wait.min(run.left()),
        run.timing.poll,
        || {
            run.halted_now()?;
            Ok((run.client.info()?["online"] == json!(true)).then_some(()))
        },
    )?;
    if online.is_none() {
        bail!(
            "the account is not online (account: {}): nothing can arrive. Read threema-run/app.log",
            run.client.info()?["account"]
        );
    }
    let (mut last, mut since) = (Snap::take(run.client)?, Instant::now());
    loop {
        run.halted_now()?;
        std::thread::sleep(run.timing.poll);
        let now = Snap::take(run.client)?;
        if now.list() != last.list() {
            since = Instant::now();
        }
        last = now;
        if since.elapsed() >= run.timing.settle {
            return Ok(last);
        }
        if Instant::now() >= deadline {
            bail!(
                "the list of conversations kept changing for {} s ({} conversations): a catch-up is still being mapped. Run the step again later",
                run.timing.baseline_wait.as_secs(),
                last.chats.len()
            );
        }
    }
}

/// Waits for exactly one new message from others in one chat of `kind` and returns the chat. A text
/// that never comes within `lost_after` of the step's start (S7: 10 minutes) is `lost`; two new
/// messages in one chat where one was sent are `twice`. The step opens no chat.
fn one_arrives(run: &mut StepRun<'_>, kind: &str, instruction: &str) -> Result<i64> {
    let (chat, facts) = one_arrives_in(run, kind, instruction, None)?;
    run.facts = Some(json!({"chats": facts}));
    Ok(chat)
}

/// [`one_arrives`] for a step that waits for several texts in one run (part two): `slot` names the
/// text, its baseline and the time waited are carried in the step's resume under that name (no slot:
/// the top level of the resume, as part one's steps have it), and the chat's facts come back for the
/// step to record.
fn one_arrives_in(
    run: &mut StepRun<'_>,
    kind: &str,
    instruction: &str,
    slot: Option<&str>,
) -> Result<(i64, Vec<Value>)> {
    let label = slot.map_or_else(String::new, |s| format!("{s}: "));
    let earlier = match slot {
        None => run.previous.clone(),
        Some(s) => run.carry.get(s).cloned(),
    };
    let before = earlier
        .as_ref()
        .and_then(|p| p["waited_ms"].as_u64())
        .map_or(Duration::ZERO, Duration::from_millis);
    let resumed = earlier.as_ref().and_then(|p| Snap::from_value(&p["base"]));
    let (base, base_newest) = match resumed {
        Some((snap, newest)) => {
            run.note(format!("{label}the baseline of the earlier call is used (a text that came between the calls counts)"));
            (snap, newest)
        }
        None => {
            let snap = settled_baseline(run)?;
            let newest = newest_message(&snap.rows);
            (snap, newest)
        }
    };
    let base_value = {
        let mut v = base.to_value();
        v["newest"] = json!(base_newest);
        v
    };
    run.personal(instruction);
    run.waiting("for the message");
    let mine = Instant::now();
    let bound = run
        .wait_for(STEP_WAIT_MAX_SECS)
        .min(run.left())
        .min(run.timing.lost_after.saturating_sub(before));
    let seen = poll(bound, run.timing.poll.max(Duration::from_millis(5)), || {
        run.halted_now()?;
        let now = Snap::take(run.client)?;
        Ok((!arrivals(&base.chats, base.open, base_newest, &now, kind).none()).then_some(()))
    })?;
    if seen.is_none() {
        let waited = before + mine.elapsed();
        if waited >= run.timing.lost_after {
            let _ = run.client.said(Said::Lost)?;
            bail!(
                "nothing arrived in a {kind} chat in {} s: the text is counted LOST and the run is halted (lost). Read threema-run/app.log; a new start needs --after-halt and his word",
                waited.as_secs()
            );
        }
        let resume = json!({
            "base": base_value,
            "waited_ms": u64::try_from(waited.as_millis()).unwrap_or(u64::MAX),
        });
        let why = format!(
            "nothing arrived in a {kind} chat in {} s of {}: run the step again, it keeps the baseline and goes on; after {} s in all the text counts lost",
            waited.as_secs(),
            run.timing.lost_after.as_secs(),
            run.timing.lost_after.as_secs()
        );
        return match slot {
            None => {
                run.resume = Some(resume);
                Err(anyhow!(why))
            }
            Some(s) => {
                run.carry_set(s, resume);
                Err(run.pause(why))
            }
        };
    }
    // A duplicate would follow at once: look again after a moment.
    std::thread::sleep(run.timing.settle);
    let now = Snap::take(run.client)?;
    let found = arrivals(&base.chats, base.open, base_newest, &now, kind);
    if !found.fresh.is_empty() {
        bail!(
            "a message opened a {kind} conversation that did not exist before ({} new): Kourier starts no chat, so the sender may not be the one the step asked for or the same person is filed twice. Stop and tell the user",
            found.fresh.len()
        );
    }
    match found.known.as_slice() {
        [(chat, 1)] => {
            run.note(format!(
                "{label}one new message, once, in {kind} conversation #{chat}"
            ));
            let facts: Vec<Value> = now
                .chats
                .iter()
                .filter(|c| c.id == *chat)
                .map(chat_fact)
                .collect();
            Ok((*chat, facts))
        }
        [(chat, n)] if *n >= 2 => {
            let _ = run.client.said(Said::Twice)?;
            bail!(
                "{n} new messages arrived in one {kind} conversation (#{chat}) where one was sent: the text is shown TWICE and the run is halted (twice). Read threema-run/app.log"
            )
        }
        other => bail!(
            "expected one new message in one existing {kind} chat, found {other:?} (conversation, count)"
        ),
    }
}

fn step_receive(run: &mut StepRun<'_>) -> Result<()> {
    one_arrives(
        run,
        "direct",
        "from the PERSONAL phone, send ONE short text to the test ID in your existing 1:1 chat with it (Kourier starts no chat: a chat that is not in its list yet is a finding)",
    )?;
    run.note("the step opens no chat: the count is the list's unread counter (opening an unread chat would mark it read)");
    Ok(())
}

fn step_group_receive(run: &mut StepRun<'_>) -> Result<()> {
    one_arrives(
        run,
        "group",
        "from the PERSONAL phone, send ONE short text in the group both phones are in",
    )?;
    Ok(())
}

// ---- Step `mirror` ----------------------------------------------------------------------------------------------

/// Step `mirror`: ONE own message the TEST phone writes to the personal ID shows once in the 1:1 chat
/// with the receipt `sent`, then `read` once he says the personal phone opened it. The chat is read
/// from the list (activity and the newest own receipt); the rows are counted only when HE opened the
/// chat in the window (the step opens none: opening a chat with unread messages marks it read).
fn step_mirror(run: &mut StepRun<'_>) -> Result<()> {
    let result = mirror_inner(run);
    // M3: once the send was tapped, a failure keeps the carry (the unique text, "send tapped", the
    // chat's activity before the send), so the next call only COUNTS and judges: it never types or
    // sends again.
    if result.is_err() && run.resume.is_none() && run.carried_bool("send_tapped") {
        let why = format!("{:#}", result.expect_err("an error"));
        return Err(run.pause(why));
    }
    result
}

fn mirror_inner(run: &mut StepRun<'_>) -> Result<()> {
    let base = settled_baseline(run)?;
    let Some(chat) = base
        .chats
        .iter()
        .filter(|c| c.kind == "direct" && !c.is_self)
        .max_by_key(|c| c.last_activity)
        .cloned()
    else {
        bail!("no 1:1 chat is listed: nothing can be mirrored (Kourier starts no chat)");
    };
    let open_rows_before = (base.open == chat.id)
        .then(|| base.rows.iter().filter(|r| r["own"] == json!(true)).count());
    // A call after the send was tapped judges against the chat's activity BEFORE the send.
    let mut chat = chat;
    if let Some(before) = run
        .carried_u64("mirror_activity")
        .and_then(|v| i64::try_from(v).ok())
    {
        chat.last_activity = before;
    }
    if run.has_phone() {
        let text = run.marker("mirror");
        if !run.carried_bool("send_tapped") {
            run.act("the step's unique mirror text is typed into the 1:1 chat on the TEST phone and sent");
            let Some(mut phone) = run.tp.take() else {
                bail!("no TEST phone");
            };
            let prepared = tphone::mirror_prepare(phone.as_mut(), &text);
            let send = match prepared {
                Ok(send) => send,
                Err(_) => {
                    run.tp = Some(phone);
                    return Err(run.pause(
                        "the mirror text could not be prepared on the TEST phone (the 1:1 chat is not the one row of its kind, the compose field or the send control is not as expected): NOTHING was sent; run `step mirror` again",
                    ));
                }
            };
            // The record comes BEFORE the tap: a call that dies after it never sends again.
            run.carry_set("send_tapped", json!(true));
            run.carry_set(
                "mirror_activity",
                json!(u64::try_from(chat.last_activity).unwrap_or(0)),
            );
            let tapped = tphone::mirror_tap(phone.as_mut(), &send);
            run.tp = Some(phone);
            if tapped.is_err() {
                bail!(
                    "the send tap was refused by the TEST phone: the text may or may not have gone; the next call only counts it and never sends again"
                );
            }
        }
        let Some(mut phone) = run.tp.take() else {
            bail!("no TEST phone");
        };
        let shown = run.shows_once(phone.as_mut(), tphone::ChatKind::Direct, &text);
        run.tp = Some(phone);
        match shown {
            tphone::Verdict::Yes => {
                run.note("the mirror text shows once in the TEST phone's 1:1 chat")
            }
            tphone::Verdict::No => bail!(
                "the mirror text does not show exactly once in the TEST phone's 1:1 chat after the send: a finding; the step does not send again"
            ),
            tphone::Verdict::Unknown(why) => bail!(
                "{why}: the step does not send again; run `step mirror` again, it only counts"
            ),
            tphone::Verdict::Ask => {
                bail!("the chat could not be judged: the step does not send again")
            }
        }
    } else {
        run.phone(
            "on the TEST phone write ONE short text to the personal ID in the existing 1:1 chat (to count the rows exactly, open that chat in the Kourier window yourself; the step opens none)",
        );
    }
    run.waiting("for the own message");
    let seen = poll(run.wait_for(300).min(run.left()), run.timing.poll, || {
        run.halted_now()?;
        let now = run.client.chats()?;
        Ok(now.into_iter().find(|c| c.id == chat.id).filter(|c| {
            c.last_activity > chat.last_activity
                && matches!(c.newest_own.as_str(), "sent" | "delivered" | "read")
        }))
    })?;
    let Some(arrived) = seen else {
        bail!(
            "no own message showed in the 1:1 chat in time (its activity did not move or the newest own message is not `sent`): read threema-run/app.log"
        );
    };
    std::thread::sleep(run.timing.settle);
    let after = Snap::take(run.client)?;
    let now_chat = after
        .chats
        .iter()
        .find(|c| c.id == chat.id)
        .cloned()
        .ok_or_else(|| anyhow!("the 1:1 chat left the list"))?;
    if now_chat.unread != chat.unread {
        bail!(
            "the 1:1 chat's unread count moved while the own message showed: a message of the peer arrived too; run the step again"
        );
    }
    if let Some(before) = open_rows_before {
        let own_now = after
            .rows
            .iter()
            .filter(|r| r["own"] == json!(true))
            .count();
        if after.open == chat.id && own_now != before + 1 {
            let _ = run.client.said(Said::Twice)?;
            bail!(
                "{} new own rows in the open chat where one was written: the run is halted (twice)",
                own_now.saturating_sub(before)
            );
        }
        run.note("the open chat's own rows grew by exactly one");
    } else {
        run.note("the chat was not open: the own message is read from the list (activity moved once, newest own receipt shown); row counts need the chat open");
    }
    run.note(format!(
        "the own message's receipt shows `{}`",
        word(&arrived.newest_own)
    ));
    run.facts = Some(json!({"chats": [chat_fact(&now_chat)]}));
    // Read, once the personal phone opened the chat. With a test phone Kourier's own state is the
    // first look: the newest own message reads `read` after he opens the chat (his act); only when it
    // does not, he is asked.
    let mut read_by_state = false;
    if run.has_phone() {
        run.personal(
            "on your personal Threema open the 1:1 chat with the test ID and look at the text from the TEST phone (read receipts on there)",
        );
        let got = poll(
            run.timing.read_wait.min(run.left()),
            run.timing.poll,
            || {
                run.halted_now()?;
                Ok(run
                    .client
                    .chats()?
                    .into_iter()
                    .find(|c| c.id == chat.id && c.newest_own == "read"))
            },
        )?;
        if got.is_some() {
            run.record_origin("pp-read-it", Origin::Kourier);
            run.note("the own message's receipt reached `read` (Kourier's state: the personal phone opened the chat)");
            read_by_state = true;
        }
    }
    let read = if read_by_state {
        None
    } else {
        run.ask_him("pp-read-it", true)?
    };
    if read_by_state {
        run.ask_the_warning()?;
        return Ok(());
    }
    match read {
        Some(true) => {
            let got = poll(
                run.timing.read_wait.min(run.left()),
                run.timing.poll,
                || {
                    run.halted_now()?;
                    Ok(run
                        .client
                        .chats()?
                        .into_iter()
                        .find(|c| c.id == chat.id && c.newest_own == "read"))
                },
            )?;
            if got.is_none() {
                bail!(
                    "the own message did not read `read` in {} s after the personal phone opened it (read receipts on there?): a finding",
                    run.timing.read_wait.as_secs()
                );
            }
            run.note("the own message's receipt reached `read`");
        }
        Some(false) => run.note("read: not asked for (the personal phone did not open the chat)"),
        None => {
            bail!("no answer to \"did the personal phone open it\" in time: run the step again")
        }
    }
    run.ask_the_warning()?;
    Ok(())
}

// ---- Step `reconnect` -------------------------------------------------------------------------------------------

/// The newest recorded facts per chat from the steps log (the latest record that has `facts.chats`
/// wins for a chat).
fn recorded_chats(steps: &[Value]) -> Vec<(i64, i64, i64)> {
    let mut found: Vec<(i64, i64, i64)> = Vec::new();
    for step in steps.iter().filter(|s| s["result"] == "PASS") {
        for fact in step["facts"]["chats"].as_array().into_iter().flatten() {
            let (Some(chat), Some(unread), Some(activity)) = (
                fact["chat"].as_i64(),
                fact["unread"].as_i64(),
                fact["last_activity"].as_i64(),
            ) else {
                continue;
            };
            found.retain(|(c, ..)| *c != chat);
            found.push((chat, unread, activity));
        }
    }
    found
}

/// The recorded steps (`steps.jsonl`), oldest first.
pub(super) fn recorded_steps(dir: &ProfileDir) -> Vec<Value> {
    std::fs::read_to_string(dir.steps_log())
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// Step `reconnect`: after `stop` and a start of the linked profile: online with no login step and
/// no close 4115; whether this socket leads is recorded; the chats of the earlier steps stand as
/// they were left (the same unread counts and activity, so no text was lost or shown twice).
fn step_reconnect(run: &mut StepRun<'_>) -> Result<()> {
    run.halted_now()?;
    if !State::load(run.dir)?.is_some_and(|s| s.linked()) {
        bail!("this profile is not linked: run `step link` first");
    }
    run.waiting("for the account to be online again");
    let online = poll(
        run.timing.online_wait.min(run.left()),
        run.timing.poll,
        || {
            run.halted_now()?;
            let info = run.client.info()?;
            Ok((info["online"] == json!(true)).then_some(info))
        },
    )?;
    let Some(info) = online else {
        bail!(
            "the account did not come online in {} s (account: {}). Do not link again blindly: read threema-run/app.log (a 4115 at connect means the server dropped the device)",
            run.timing.online_wait.as_secs(),
            run.client.info()?["account"]
        );
    };
    let (events, _) = run.client.events(0)?;
    let logins = events
        .iter()
        .filter(|e| matches!(e["event"].as_str(), Some("login_step" | "login_qr")))
        .count();
    if logins > 0 || info["login_seen"] == json!(true) {
        bail!(
            "the app showed {logins} login steps since it started: it did not simply reconnect. Do not link again blindly: read threema-run/app.log"
        );
    }
    let (lines, _) = run.client.lines(0)?;
    if lines
        .iter()
        .any(|l| matches!(l.parsed.body, Body::Closed { code: 4115 }))
    {
        bail!(
            "the mediator closed a socket with 4115 since the start (the device was dropped?): do not link again blindly"
        );
    }
    run.note("online again without a login step and without a 4115");
    let leads = info["leader"] == json!(true) || info["lines"]["D1_UNBLOCKED"].as_u64() > Some(0);
    run.note(format!(
        "this socket {}: {}",
        if leads {
            "LEADS (D1 seen)"
        } else {
            "does not lead (no D1)"
        },
        "who leads after Kourier came back is recorded, not judged"
    ));
    // With a test phone the kept link's label is looked at on the phone (the device info the phone
    // could not read before the device-info fix): recorded, a finding when it is not there, no halt.
    if run.has_phone() {
        match run.auto_answer("list-shows-kourier") {
            Ok(Some(shown)) => run.note(format!(
                "the TEST phone's linked-devices list shows the Kourier entry: {}",
                if shown {
                    "yes"
                } else {
                    "NO (a finding: the label or the device info)"
                }
            )),
            Ok(None) => {}
            Err(e) => run.note(format!("the TEST phone's list was not read: {e:#}")),
        }
    }
    // The chats of the earlier steps.
    let recorded = recorded_chats(&recorded_steps(run.dir));
    if recorded.is_empty() {
        run.note(
            "no earlier step recorded a chat: the texts of the earlier steps were not compared",
        );
    } else {
        let chats = run.client.chats()?;
        let mut moved = Vec::new();
        for (id, unread, activity) in &recorded {
            match chats.iter().find(|c| c.id == *id) {
                None => moved.push(format!("#{id} is gone from the list")),
                Some(c) if c.unread != *unread => moved.push(format!(
                    "#{id} shows {} unread, the step left {unread}",
                    c.unread
                )),
                Some(c) if c.last_activity < *activity => {
                    moved.push(format!("#{id}'s activity is older than the step left it"));
                }
                Some(_) => {}
            }
        }
        if !moved.is_empty() {
            bail!(
                "the chats of the earlier steps do not stand as they were left ({}): a text may be lost or shown twice, or a later text came; look at the window",
                moved.join("; ")
            );
        }
        run.note(format!(
            "{} chats of the earlier steps stand as they were left (unread counts equal, activity not older)",
            recorded.len()
        ));
    }
    Ok(())
}

// ---- Step `passive` ----------------------------------------------------------------------------------------------

/// Step `passive --seconds N` (default 600): the TEST phone unlocked with Threema in front, Kourier
/// sends nothing of its own: the account is online at every look, no close, no halt, no error; the
/// count lines read hold follower words only (or the leader's within the judge's bounds). A call
/// watches at most 540 s; a longer one ends FAIL with a `resume` and goes on in the next call.
fn step_passive(run: &mut StepRun<'_>) -> Result<()> {
    let wanted = run.args.seconds.unwrap_or(600);
    if wanted == 0 {
        bail!("--seconds 0 watches nothing: give a number of seconds, 1 or more");
    }
    let done = run
        .previous
        .as_ref()
        .and_then(|p| p["done_ms"].as_u64())
        .map_or(Duration::ZERO, Duration::from_millis);
    let (events_from, lines_from) = match &run.previous {
        Some(p) => (
            p["events_from"].as_u64().unwrap_or(0),
            p["lines_from"].as_u64().unwrap_or(0),
        ),
        None => {
            let info = run.client.info()?;
            (
                info["next_seq"].as_u64().unwrap_or(0),
                info["next_line"].as_u64().unwrap_or(0),
            )
        }
    };
    let mut looks = run
        .previous
        .as_ref()
        .and_then(|p| p["looks"].as_u64())
        .unwrap_or(0);
    let remaining = Duration::from_secs(wanted).saturating_sub(done);
    let this_call = remaining.min(run.left());
    run.waiting(&format!(
        "{} s more with the TEST phone unlocked and Threema in front, sending nothing ({} of {wanted} s done)",
        this_call.as_secs(),
        done.as_secs()
    ));
    let deadline = Instant::now() + this_call;
    while Instant::now() < deadline {
        run.halted_now()?;
        let info = run.client.info()?;
        looks += 1;
        if info["online"] != json!(true) {
            bail!(
                "the account left online after {looks} looks (account: {})",
                info["account"]
            );
        }
        let nap = run
            .timing
            .look
            .min(deadline.saturating_duration_since(Instant::now()));
        std::thread::sleep(nap);
    }
    let watched = done + this_call;
    if watched < Duration::from_secs(wanted) {
        run.resume = Some(json!({
            "done_ms": u64::try_from(watched.as_millis()).unwrap_or(u64::MAX),
            "looks": looks,
            "events_from": events_from,
            "lines_from": lines_from,
        }));
        bail!(
            "this call's time ran out at {} of {wanted} s: run `step passive --seconds {wanted}` again, it goes on",
            watched.as_secs()
        );
    }
    let (events, _) = run.client.events(events_from)?;
    let errors: Vec<String> = events
        .iter()
        .filter(|e| e["event"] == "error")
        .map(|e| format!("{} {}", e["data"]["source"], e["data"]["kind"]))
        .collect();
    if !errors.is_empty() {
        bail!("the app reported errors meanwhile: {}", errors.join("; "));
    }
    let (lines, _) = run.client.lines(lines_from)?;
    let closes = lines
        .iter()
        .filter(|l| matches!(l.parsed.body, Body::Closed { .. }))
        .count();
    if closes > 0 {
        bail!(
            "the mediator closed the socket {closes} times while the step watched: the run is not passive"
        );
    }
    let counts = counts_of(&lines);
    match counts.last() {
        Some(c) => {
            run.note(format!("last count line: {}", counts_note(c)));
            let leads = run.client.info()?["leader"] == json!(true);
            if !leads && !follower_words_only(c) {
                bail!(
                    "a count line holds words a follower may not say while no leader promotion was seen"
                );
            }
            run.note(if leads {
                "this socket leads: the counts are judged within the leader's bounds by the run"
            } else {
                "the count lines hold follower words only"
            });
        }
        None => run.note("frame counts: not measured (no count line since the step began)"),
    }
    let chats = run.client.chats()?;
    run.note(format!(
        "online at each of {looks} looks over {wanted} s; {} chats listed",
        chats.len()
    ));
    Ok(())
}

mod auto;
mod part2;
mod sends;

// ---- The step table and the runner ------------------------------------------------------------------------

type StepFn = fn(&mut StepRun<'_>) -> Result<()>;

pub(super) struct StepDef {
    pub name: &'static str,
    pub what: &'static str,
    run: StepFn,
}

/// The steps, in the order of the run (part one's seven, then the media and sends [`sends`] and part
/// two's: [`part2`]).
pub(super) const STEPS: &[StepDef] = &[
    StepDef {
        name: "link",
        what: "the first link by QR code in a start made for it: you answer that the phone in your hand is the TEST phone, scan, compare the three emoji, and answer what the TEST phone's list shows",
        run: step_link,
    },
    StepDef {
        name: "sync",
        what: "the account goes syncing, then online; the time from the link; the chats; no leader promotion while the phone leads; the first frame counts hold follower words only",
        run: step_sync,
    },
    StepDef {
        name: "receive",
        what: "one text from the PERSONAL phone into the existing 1:1 chat arrives once (never a new chat; the step opens no chat)",
        run: step_receive,
    },
    StepDef {
        name: "group-receive",
        what: "one text from the PERSONAL phone into the group both phones are in arrives once",
        run: step_group_receive,
    },
    StepDef {
        name: "mirror",
        what: "one text you write ON THE TEST PHONE to the personal ID shows once as an own message, `sent`, then `read` once the personal phone opened it",
        run: step_mirror,
    },
    StepDef {
        name: "reconnect",
        what: "after `stop` and a start without --for-link: online again with no login step and no 4115; the chats of the earlier steps stand as they were left",
        run: step_reconnect,
    },
    StepDef {
        name: "passive",
        what: "sends nothing for --seconds (600): online at every look, no close, no error, follower words only (a call watches 540 s; run it again to go on)",
        run: step_passive,
    },
    // Part two (T7). The order is the run's: `kinds` (his word), the phone's sleep, the flight mode,
    // then the two ends (each only on his word; one of them).
    StepDef {
        name: "kinds",
        what: "optional, his word: the PERSONAL phone sends a quote, a mention in the group, bold text, an edit, a reaction and a delete while the TEST phone leads; Kourier only receives and counts",
        run: part2::step_kinds,
    },
    // The media and the sends (TH8): after `passive`, whose follower words allow no frame of a send.
    // The window is the user's to act in; the no-send rule is lifted only inside these three.
    StepDef {
        name: "files",
        what: "a picture, a file and a voice message from the PERSONAL phone arrive in the 1:1 chat, open and play in the window; the profile picture shows if there is one",
        run: sends::step_files,
    },
    StepDef {
        name: "send",
        what: "one text you send in the Kourier window reaches the PERSONAL phone exactly once and shows on the TEST phone as its own",
        run: sends::step_send,
    },
    StepDef {
        name: "group-send",
        what: "one text you send in the Kourier window into the group reaches the PERSONAL phone exactly once and shows on the TEST phone as its own",
        run: sends::step_group_send,
    },
    StepDef {
        name: "send-file",
        what: "one small picture with a caption you send from the Kourier window reaches the PERSONAL phone exactly once, opens there, and shows on the TEST phone as its own",
        run: sends::step_send_file,
    },
    StepDef {
        name: "phone-sleep",
        what: "the TEST phone locked for 10 minutes, one text from the PERSONAL phone at minute 5: once in Kourier, once on the TEST phone; records whether Kourier took the lead in the sleep and the push mode",
        run: part2::step_phone_sleep,
    },
    StepDef {
        name: "flight-mode",
        what: "THE LEADER TEST: the TEST phone in flight mode, Kourier takes the lead (up to 10 minutes; a lead from the sleep is read first), one 1:1 and one group text from the PERSONAL phone, once each; flight mode off; a later text; the counts and the phones' words. As the leader Kourier writes the unblock once, per incoming message a Reflect and a message-ack, per 1:1 text the receipt (a Reflect and one box) and per forward-security-wrapped message one Reject box",
        run: part2::step_flight_mode,
    },
    StepDef {
        name: "removed-on-phone",
        what: "END A, only on his word: you remove the Kourier entry on the TEST phone while the run lives; the mediator closes with 4113 and the account reads logged out",
        run: part2::step_removed_on_phone,
    },
    StepDef {
        name: "unlink",
        what: "END B, only on his word: you remove the account in Kourier's Settings; Kourier drops its device on the mediator (acked), the account goes, and the TEST phone's list is clean",
        run: part2::step_unlink,
    },
];

/// `steps`: the list, names and words only.
pub(super) fn list_steps() -> String {
    let mut out = String::from("steps of the run, in order:\n");
    for def in STEPS {
        out.push_str(&format!("  {:<14} {}\n", def.name, def.what));
    }
    out
}

/// `steps` as the command line runs it.
pub(super) fn steps_command(_a: &Args) -> Result<i32> {
    print!("{}", list_steps());
    Ok(0)
}

/// A note or a reason as the record keeps it: redacted, one line, no `|`.
pub(super) fn safe_text(text: &str) -> String {
    redact(text).replace('|', "/").replace('\n', " ")
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// Waits until the run's app answers on its dev channel (or the run is halted: the step then says
/// so). A start that waits for the ID says so at once.
fn ready_for_a_step(client: &Client, bound: Duration, poll_every: Duration) -> Result<()> {
    let deadline = Instant::now() + bound;
    loop {
        let info = client.info().context(
            "no run answers on this profile: `threema-run start` first (see `threema-run state`)",
        )?;
        if info["halted"].is_string() || info["ready"] == json!(true) {
            // LOW-7: an app whose environment or memory map this user's other processes can read is
            // no app a step may go on with (the run is halted `app-readable` then; say it anyway).
            if info["app_readable"] == json!(true) {
                bail!(
                    "the app's environment can be read by other processes of this user: stop the run and tell the user"
                );
            }
            return Ok(());
        }
        if info["id"] == json!("waiting") {
            bail!("{ID_NOT_TYPED}");
        }
        if info["exit"].is_string() {
            bail!("the app has exited: read threema-run/app.log, then `threema-run state`");
        }
        if Instant::now() >= deadline {
            bail!("the app's dev channel did not answer in time: read threema-run/app.log");
        }
        std::thread::sleep(poll_every);
    }
}

/// What one call of a step came to.
#[derive(Debug)]
pub(super) struct StepEnd {
    /// 0 for a pass, 1 for a failure.
    pub status: i32,
    /// The three below are the record's words: the tests read them (the runner writes the record).
    #[cfg_attr(not(test), allow(dead_code))]
    pub reason: String,
    #[cfg_attr(not(test), allow(dead_code))]
    pub notes: Vec<String>,
    /// What was said to the user.
    #[cfg_attr(not(test), allow(dead_code))]
    pub said: Vec<String>,
}

/// The latest unfinished call's `resume` for a step: the `resume` of its latest record, while that
/// record is a failure that carries one.
fn resume_of(steps: &[Value], name: &str) -> Option<Value> {
    let last = steps.iter().rev().find(|s| s["name"] == name)?;
    if last["result"] != "FAIL" {
        return None;
    }
    last.get("resume").filter(|r| r.is_object()).cloned()
}

/// Runs one step and writes it down (wa_run.rs:10042 `run_step`).
pub(super) fn run_step(
    a: &Args,
    dir: &ProfileDir,
    client: &Client,
    def: &StepDef,
    timing: StepTiming,
    heard: &dyn Fn(&str),
) -> Result<StepEnd> {
    run_step_with(a, dir, client, def, timing, heard, None)
}

/// [`run_step`] with a TEST phone handed in (the tests' scripted one); without it the phone is
/// opened from the profile's `test_phone` value, if the profile has one.
pub(super) fn run_step_with(
    a: &Args,
    dir: &ProfileDir,
    client: &Client,
    def: &StepDef,
    timing: StepTiming,
    heard: &dyn Fn(&str),
    injected: Option<Result<Box<dyn Phone>>>,
) -> Result<StepEnd> {
    println!(
        "STEP {} on profile {}{}: {}",
        def.name,
        dir.label(),
        if dir.rehearsal { " (rehearsal)" } else { "" },
        def.what
    );
    let started = now_ms();
    let previous = resume_of(&recorded_steps(dir), def.name);
    let mut carry = previous
        .as_ref()
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let obs = carry
        .remove("obs")
        .and_then(|o| o.as_object().cloned())
        .unwrap_or_default();
    // The TEST phone of the run, if it has one: opened (connected, woken) before the step starts.
    let mut tp: Option<Box<dyn Phone>> = None;
    let mut open_failed: Option<&str> = None;
    let failed = "the TEST phone could not be reached (adb connect / get-state): nothing was decided; fix the link to it, then run this step again";
    match injected {
        Some(Ok(p)) => tp = Some(p),
        Some(Err(_)) => open_failed = Some(failed),
        None => {
            // N3: the serial is the SUPERVISOR's copy (what `start` printed), not `state.json`.
            if let Some(serial) = client
                .info()
                .ok()
                .and_then(|i| i["test_phone_serial"].as_str().map(str::to_owned))
            {
                match tphone::open_phone(dir, &serial) {
                    Ok(p) => tp = Some(p),
                    Err(_) => open_failed = Some(failed),
                }
            }
        }
    }
    let mut run = StepRun {
        client,
        dir,
        args: a,
        timing,
        heard,
        notes: Vec::new(),
        said: Vec::new(),
        previous,
        resume: None,
        facts: None,
        carry,
        obs,
        started: Instant::now(),
        step: def.name,
        tp,
        origins: Vec::new(),
    };
    let result = match open_failed {
        // M1: an unreachable phone is a pause that KEEPS what the earlier calls carried (a lock clock,
        // a cut on record), never a start from nothing.
        Some(why) => Err(run.pause(why)),
        None => ready_for_a_step(client, timing.ready_wait, timing.poll)
            .and_then(|()| (def.run)(&mut run)),
    };
    let ended = now_ms();
    // What a step of part two saw goes into the record's facts (the report reads it).
    if !run.obs.is_empty() {
        let mut facts = run.facts.take().unwrap_or_else(|| json!({}));
        facts["obs"] = Value::Object(run.obs.clone());
        run.facts = Some(facts);
    }
    // Where each answer of this call came from: the report names them (D2).
    if !run.origins.is_empty() {
        let mut facts = run.facts.take().unwrap_or_else(|| json!({}));
        facts["origins"] = Value::Array(run.origins.clone());
        run.facts = Some(facts);
    }
    let (verdict, reason) = match &result {
        Ok(()) => ("PASS", String::new()),
        Err(e) => ("FAIL", safe_text(&format!("{e:#}"))),
    };
    for note in &run.notes {
        println!("NOTE: {}", safe_text(note));
    }
    println!(
        "{verdict} {} ({}.{} s){}",
        def.name,
        (ended - started) / 1000,
        (ended - started) % 1000 / 100,
        if reason.is_empty() {
            String::new()
        } else {
            format!(": {reason}")
        }
    );
    append_line(
        &dir.steps_log(),
        &json!({
            "name": def.name,
            "result": verdict,
            "reason": reason,
            "started_ms": started,
            "ended_ms": ended,
            "notes": run.notes.iter().map(|n| safe_text(n)).collect::<Vec<_>>(),
            "resume": run.resume,
            "facts": run.facts,
        })
        .to_string(),
    )?;
    Ok(StepEnd {
        status: i32::from(result.is_err()),
        reason,
        notes: run.notes,
        said: run.said,
    })
}

/// `step <name>` as the command line runs it.
pub(super) fn step_command(a: &Args) -> Result<i32> {
    let name = a.step.as_deref().unwrap_or_default();
    // A step name is never echoed: a lower-case ID would have its shape.
    let def = STEPS.iter().find(|d| d.name == name).ok_or_else(|| {
        anyhow!(
            "unknown step (not shown: it might hold the ID; see `cargo xtask threema-run steps`)"
        )
    })?;
    let dir = a.place.resolve(&var, &target_dir())?;
    let socket = socket_for_client(&dir, &var)?;
    if !socket.exists() {
        bail!(
            "no run is started on this profile: `threema-run start` first (see `threema-run state`)"
        );
    }
    let timing = if dir.rehearsal {
        StepTiming::REHEARSAL
    } else {
        StepTiming::REAL
    };
    Ok(run_step(a, &dir, &Client::at(socket), def, timing, &|_| {})?.status)
}
