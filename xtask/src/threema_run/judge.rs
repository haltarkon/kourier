//! THE JUDGE (T5): the stop rules S1-S12 of `briefs/tr.md` §2.10 with the critic's changes, as ONE
//! function over what the supervisor sees, in order: [`judge`] takes the run's context ([`Judge`],
//! what the earlier lines and events left), one thing seen ([`Seen`]) and its time, and says the
//! halt's kind word, or nothing. It reads no file, no clock and no process: every rule has its table
//! test (`tests/judge.rs`), on the stand-in app's own lines.
//!
//! Every halt STOPS the run (the supervisor's [`super::supervise`] `end_the_run`: a link the phone
//! confirmed is let finish, a login that waits at its code is closed, the drop of a link that failed
//! after its hello is waited for (M2), then the app is asked to quit; the switch goes off after its
//! exit). A halt names its kind only, never a value. The first cause stays.
//!
//! THE RULES (the kind words in brackets):
//!
//! - S1a: a login that FAILED at `qr` or `emoji`, or the refusal of the ID (`NotTheExpectedAccount`)
//!   at `linking` [`link-refused`]; at `qr` the refusal is B1's (the list was empty: no socket)
//!   [`list-not-taken`]. The failed step comes first, its `error` event (the kind word) right after:
//!   the decision waits for that event (or for the next event, or the app's exit, as a belt).
//! - S1b: any other failure at `linking`, and the app gone (a crash, a kill, a stop, an actor exit)
//!   while the login stood at `linking` (RP-b's review S-1) [`link-failed-after-hello`]. At a failed
//!   step the core's logout drops the device on a socket of its own: [`Judge::drop_owed_since_ms`]
//!   tells the end to wait for D4's outcome (critic M2). The question of the keeper root stays the
//!   supervisor's (`link_at`).
//! - S2: he says the emoji differ and cancels on the phone: the login fails at `emoji` (S1a).
//! - S3: `conn` `needs_action` [`needs-action`], `restricted` [`restricted`], an account or `conn`
//!   `logged_out` [`logged-out`] but while a removal is asked (`unlink`, `removed-on-phone`; R-L2).
//! - S4: E1 with 4110-4115 (but 4113 while a removal is asked, and 4113/4114 after the drop went on
//!   the live socket) or a code the spec does not name [`close-<code>`]; a CSP `close-error`
//!   (`in_close_error`) [`close-error`]; E3 [`untrusted`]; E7 [`not-registered`]; E8
//!   [`first-row-refused`].
//! - S5a, a FOLLOWER socket (its count line has `out_unblock` 0): a chat server message read
//!   (`in_csp_message`) or the H1 guard's WARN [`unasked`]; any write but `reflected_ack`, `echo`,
//!   `idle`, the frames of a `job` (R-H2: a job left from a leader socket may go on a follower; at
//!   most two reflects per box, one row in flight) and `drop` while a removal is asked
//!   [`follower-wrote`].
//! - S5b, a LEADER socket (`out_unblock` >= 1, or D1 seen on it): `out_unblock` > 1 or a second D1;
//!   `out_message_ack`, `out_reflect_inbound` or `out_ephemeral` above `in_csp_message`; boxes above
//!   three per message; a job's reflects above two per box (one row in flight); `out_other`; `drop`
//!   but while a removal is asked [`leader-burst`].
//! - Any socket: a frame of origin `transmit` (no user send exists before TH7, `NotYetBuilt`)
//!   [`transmit`].
//! - S5c: an L-KIND line [`unhandled-<kind>`] (and `in_other` > 0 without one [`unhandled-other`]).
//! - S5d: a D2 line or `held` > 0 [`held-<rule>`]: a held group change (`group-sync-needed`,
//!   `group-held-before`) STOPS the run safely (his TH6-Q1 answer), so the phone leads again and takes
//!   it from the chat server; so does every other hold word of `th6-lines.md` ("After L6", "After the
//!   fix round (TH6 FIX-A)") but the three [`TOLERATED_HOLDS`] (`csp-unknown-type`,
//!   `creator-answer-needs-picture`: what only the phone resolves and Kourier's T-H1 step-aside hands
//!   over to it; `inbound-known`: a second copy of a known message, harmless). The store failure
//!   (`inbound-reflected-zone-failed`) and the full job budget (`inbound-reflected-queue-full`) stop.
//!   A word the tool does not know stops.
//! - THE STEP-ASIDE (TH6 L6, T-H1; after the tolerated hold the leader asks the mediator for the devices
//!   (`out_other`, answer `in_other` with the L-KIND line `mediator DevicesInfo`) and may close 1000
//!   ONCE after the INFO line [`L_STEP_ASIDE`](super::lines::L_STEP_ASIDE)): none of those is a stop. The
//!   kind line, `in_other` (up to `out_other`) and `out_other` (up to one ask a minute since the hold,
//!   and two) belong to the step-aside while a hold the phone resolves ([`PHONE_HOLDS`]; not
//!   `inbound-known`) stands on the socket or its line was read; without one they stop as before (`unhandled-DevicesInfo`, `leader-burst`,
//!   `follower-wrote`). The step-aside's own end is no loop for S6, and [`Judge::asides`] counts them
//!   for the report.
//! - S6: more than 3 sockets ended in 10 minutes, or more than 13 in an hour, that no stop caused
//!   [`socket-loop`]. A socket's end is its `when=end` count line (E1 comes before it on the same
//!   socket and is not counted again); an end while the run is ending is the stop's.
//! - S7: a text lost or doubled (a step's verdict) [`lost`] / [`twice`].
//! - S8: an ERROR line of `kourier_threema` or `kourier_core`, E5, a panic [`error`]; the app's exit
//!   nobody asked for [`exited`].
//! - S9: he answers that a phone warns (unofficial client, the device, verify the account)
//!   [`phone-warning`].
//! - S10: E6 [`list-not-taken`]. S11: E2 [`clock`]. S12: he says stop (a step's) [`stopped`].
//! - A contract line whose fields the tool cannot read [`line-unreadable`]: a blind judge is no run.
//! - MEDIUM-1 (security review): the same for a judge that never gets a line: in a REAL place, an
//!   account that is online with no count line for [`COUNT_LINES_WITHIN`] [`count-lines-missing`]
//!   ([`count_lines_missing`]; the supervisor asks it every poll, a rehearsal's fake prints none).
//! - MEDIUM-2: a FOLLOWER's job boxes are bounded run-wide: what it wrote plus what every ended
//!   socket wrote may not pass three per message taken as the leader, and three more
//!   ([`Judge::leader_in_total`], [`Judge::job_boxes_total`]) [`follower-wrote`].
//!
//! Not halts (kept): E4, D1's first line, D4's lines (they set [`Judge::drop`]), the step-aside line and
//! a tolerated hold, the join's own words.

use std::collections::VecDeque;

use super::lines::{Body, Count, NAMED_CLOSES, Parsed, ROW_ERROR, ROW_PANIC};

/// The hold words of `th6-lines.md` "After L6" and "After the fix round (TH6 FIX-A)" (the backend's
/// `leader::steps::RULES` and its own rules): the twin test reads the backend's sources for each of
/// them. `csp-sender-lookup` and `inbound-jobs-pending` are gone and appear nowhere.
#[cfg(test)]
pub(super) const HOLD_RULES: [&str; 12] = [
    "csp-unknown-type",
    "group-sync-needed",
    "group-held-before",
    "creator-answer-needs-picture",
    "fifo-full",
    "inbound-reflect-unanswered",
    "inbound-zone-failed",
    "inbound-queue-full",
    "directory-down",
    "inbound-reflected-zone-failed",
    "inbound-reflected-queue-full",
    "inbound-known",
];

/// The holds that do not stop the run: the two only the phone resolves and T-H1's step-aside hands over
/// to it ([`PHONE_HOLDS`]), and `inbound-known` (TH6 FIX-A: a second copy of a message that already has
/// its row, held on this socket and resumed by the next; the chat server keeps it, so it is harmless
/// and may come around a reconnect). The other nine words stop: the group changes (his TH6-Q1 answer),
/// Kourier's own failures (`inbound-zone-failed`, `inbound-reflect-unanswered`,
/// `inbound-reflected-zone-failed`: a store failure; `inbound-queue-full`,
/// `inbound-reflected-queue-full`: the job budget is full, which a run of RT1 must never reach) and
/// what resolves later on Kourier's own socket (`fifo-full`, `directory-down`). The lane lead's call to
/// flip: an empty list stops on every hold.
pub(super) const TOLERATED_HOLDS: [&str; 3] = [
    "csp-unknown-type",
    "creator-answer-needs-picture",
    "inbound-known",
];

/// The tolerated holds the step-aside hands to the phone (`leader::FOR_THE_PHONE` less the group
/// changes and Kourier's own failures): only they open the step-aside's allowance (the `GetDevicesInfo`
/// ask and its answer). `inbound-known` is tolerated but never makes the leader ask, so a mediator
/// answer or an `out_other` with only that hold on the socket still stops.
pub(super) const PHONE_HOLDS: [&str; 2] = ["csp-unknown-type", "creator-answer-needs-picture"];

/// How often the leader may ask the mediator for the devices while a hold stands (`leader::ASK_AGAIN`).
const ASK_EVERY_MS: u64 = 60 * 1000;

/// What a step or his answer says (S7, S9, S12): T6 and T7 hand it to the supervisor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Said {
    Lost,
    Twice,
    PhoneWarning,
    Stop,
}

impl Said {
    /// The word a step puts on the supervisor's socket (`said {what}`), T6.
    pub(super) fn word(self) -> &'static str {
        match self {
            Self::Lost => "lost",
            Self::Twice => "twice",
            Self::PhoneWarning => "phone-warning",
            Self::Stop => "stop",
        }
    }

    /// The thing a word says, if it is one of the four.
    pub(super) fn from_word(word: &str) -> Option<Self> {
        [Self::Lost, Self::Twice, Self::PhoneWarning, Self::Stop]
            .into_iter()
            .find(|s| s.word() == word)
    }
}

/// One thing the supervisor saw, in order (the events in the words the reader kept).
#[derive(Clone, Copy, Debug)]
pub(super) enum Seen<'a> {
    /// A line of the table or a belt, parsed.
    Line(&'a Parsed),
    /// `login_step {step, kind}`.
    LoginStep {
        step: &'a str,
        kind: &'a str,
    },
    /// `error {source: login}` with its kind word (the word before the message's first `:`).
    LoginError {
        kind: &'a str,
    },
    /// `account_state {state, conn}`.
    Account {
        state: &'a str,
        conn: &'a str,
    },
    /// The app's process is gone; `asked` when a stop or an end asked it to go.
    AppExited {
        asked: bool,
    },
    Said(Said),
}

/// The run's context, as the earlier lines and events left it.
#[derive(Clone, Debug, Default)]
pub(super) struct Judge {
    /// The last login step and its kind.
    pub login: Option<(String, String)>,
    /// A login step that failed and whose `error` event has not come yet.
    pending_failure: Option<String>,
    /// S1b at a failed step: the core's logout drops the device now (critic M2), since this time.
    pub drop_owed_since_ms: Option<u64>,
    /// The drop's outcome as D4 said it: `removed` or `not-reached`.
    pub drop: Option<&'static str>,
    /// D4 "went on the live socket": a 4113 or 4114 after it is the drop, not a halt.
    drop_went: bool,
    /// D1 lines on the socket that stands now.
    d1_this_socket: u32,
    /// The socket that stands now leads (D1 seen on it).
    pub leader: bool,
    /// The times of the sockets' ends that count for S6 (the last hour).
    ends: VecDeque<u64>,
    /// The run is ending (a stop or a halt): a socket's end is the end's.
    pub ending: bool,
    /// A removal is asked (T7's `unlink` or `removed-on-phone`, R-L2): 4113, `logged_out` and the
    /// drop frame are its own.
    pub removal_asked: bool,
    /// A tolerated hold stands on the socket that stands now (a D2 line or a count line said so).
    phone_hold: bool,
    /// When the socket's first tolerated hold was seen: the step-aside's asks are bounded from it.
    hold_since_ms: u64,
    /// The step-aside's INFO line was read on the socket that stands now.
    aside_this_socket: bool,
    /// How many step-asides the run has seen (the report names them).
    pub asides: u32,
    /// MEDIUM-2: the incoming chat server messages every ENDED socket took as the leader
    /// (`in_csp_message` of its `when=end` line).
    leader_in_total: u64,
    /// MEDIUM-2: the job boxes every ENDED socket wrote (`out_box_job` of its `when=end` line).
    job_boxes_total: u64,
    /// MEDIUM-1: since when the account stands online (`active` and `online`); `None` when it does
    /// not.
    online_since_ms: Option<u64>,
    /// MEDIUM-1: when the latest count line (any `when`) was read.
    last_count_ms: Option<u64>,
}

impl Judge {
    /// A tolerated hold stands on this socket: remember since when.
    fn tolerate_hold(&mut self, at_ms: u64) {
        if !self.phone_hold {
            self.phone_hold = true;
            self.hold_since_ms = at_ms;
        }
    }

    /// Whether the leader's `GetDevicesInfo` and its answer are the step-aside's: a tolerated hold
    /// stands on the socket, or its step-aside line was read.
    fn asked_for_devices(&self) -> bool {
        self.phone_hold || self.aside_this_socket
    }
}

/// MEDIUM-1: the longest an online account may go without a count line: the line comes every 60 s of
/// the actor's clock while a socket stands (`th6-lines.md` L-COUNT), so two periods and a margin.
pub(super) const COUNT_LINES_WITHIN: std::time::Duration = std::time::Duration::from_secs(150);

/// MEDIUM-1: the halt of a BLIND judge. Every count rule (S4's close-error, S5a-d, S6) reads count
/// lines; an account that is online and has said none for `bound_ms` since the later of its going
/// online and its latest count line means the lines do not reach the tool (a log filter or a log path
/// that drops them, a backend that stopped printing them): the run must not go on unjudged. The
/// caller asks it in a real place only (the fake of a rehearsal prints no count line).
pub(super) fn count_lines_missing(j: &Judge, now_ms: u64, bound_ms: u64) -> Option<String> {
    let since = j.online_since_ms?;
    let from = j.last_count_ms.map_or(since, |last| last.max(since));
    (now_ms.saturating_sub(from) >= bound_ms).then(|| "count-lines-missing".to_owned())
}

const TEN_MINUTES_MS: u64 = 10 * 60 * 1000;
const HOUR_MS: u64 = 60 * 60 * 1000;
/// S6: more than this many ends in ten minutes is a loop.
const ENDS_IN_TEN_MINUTES: usize = 3;
/// S6: more than this many ends in an hour is a loop.
const ENDS_IN_AN_HOUR: usize = 13;

const NOT_THE_EXPECTED: &str = "NotTheExpectedAccount";

/// THE JUDGE: the halt's kind word for `seen` at `at_ms`, given what came before (`j`, updated).
pub(super) fn judge(j: &mut Judge, seen: &Seen<'_>, at_ms: u64) -> Option<String> {
    // A failed login step's decision waits for its error event; any other EVENT decides it without
    // the kind (a line may come between: it is read by another thread and does not decide).
    if !matches!(
        seen,
        Seen::Line(_) | Seen::LoginError { .. } | Seen::Said(_)
    ) && let Some(step) = j.pending_failure.take()
    {
        return Some(failed_login(j, &step, "", at_ms));
    }
    match seen {
        Seen::Line(p) => line(j, p, at_ms),
        Seen::LoginStep { step, kind } => {
            j.login = Some(((*step).to_owned(), (*kind).to_owned()));
            if *kind == "failed" {
                j.pending_failure = Some((*step).to_owned());
            }
            None
        }
        Seen::LoginError { kind } => {
            let step = j
                .pending_failure
                .take()
                .or_else(|| j.login.as_ref().map(|(s, _)| s.clone()))?;
            Some(failed_login(j, &step, kind, at_ms))
        }
        Seen::Account { state, conn } => {
            // MEDIUM-1: the count lines are owed from the moment the account is online.
            if *state == "active" && *conn == "online" {
                j.online_since_ms.get_or_insert(at_ms);
            } else {
                j.online_since_ms = None;
            }
            match (*state, *conn) {
                (_, "needs_action") => Some("needs-action".to_owned()),
                (_, "restricted") => Some("restricted".to_owned()),
                ("logged_out", _) | (_, "logged_out") if !j.removal_asked => {
                    Some("logged-out".to_owned())
                }
                _ => None,
            }
        }
        Seen::AppExited { asked } => {
            let at_linking = j.login.as_ref().is_some_and(|(step, kind)| {
                step == "linking" && kind != "failed" && kind != "done"
            });
            if at_linking {
                Some("link-failed-after-hello".to_owned())
            } else if *asked {
                None
            } else {
                Some("exited".to_owned())
            }
        }
        Seen::Said(said) => Some(
            match said {
                Said::Lost => "lost",
                Said::Twice => "twice",
                Said::PhoneWarning => "phone-warning",
                Said::Stop => "stopped",
            }
            .to_owned(),
        ),
    }
}

/// S1a / S1b for a login that failed at `step` with the error's kind word (`""`: not seen).
fn failed_login(j: &mut Judge, step: &str, kind: &str, at_ms: u64) -> String {
    match step {
        "qr" if kind == NOT_THE_EXPECTED => "list-not-taken".to_owned(),
        "qr" | "emoji" => "link-refused".to_owned(),
        "linking" if kind == NOT_THE_EXPECTED => "link-refused".to_owned(),
        // Any other failure at `linking` (and at a step Threema does not have: the cautious word):
        // the device MAY be registered; the core's logout drops it now.
        _ => {
            j.drop_owed_since_ms.get_or_insert(at_ms);
            "link-failed-after-hello".to_owned()
        }
    }
}

/// The rules of one line.
fn line(j: &mut Judge, p: &Parsed, at_ms: u64) -> Option<String> {
    let halt = |kind: &str| Some(kind.to_owned());
    if p.row == "L_COUNT" {
        j.last_count_ms = Some(at_ms);
    }
    match (p.row, &p.body) {
        // Halts by the line alone (its fields are not read).
        ("L_UNASKED", _) => halt("unasked"),
        ("E2_CLOCK", _) => halt("clock"),
        ("E3_UNTRUSTED", _) => halt("untrusted"),
        ("E5_PANICKED", _) | (ROW_PANIC, _) => halt("error"),
        ("E7_NOT_NEW", _) => halt("not-registered"),
        ("E8_FIRST_ROW", _) => halt("first-row-refused"),
        ("E6_LIST_NOT_USED", _) => halt("list-not-taken"),
        (ROW_ERROR, Body::Belt { error_of }) => matches!(
            error_of.as_deref(),
            Some("kourier_threema" | "kourier_core")
        )
        .then(|| "error".to_owned()),
        // The lines whose fields a rule reads.
        ("L_KIND", Body::Kind { server, kind }) => {
            // The step-aside's own ask: the mediator's answer is no surprise (see the module doc).
            let asked = server == "mediator" && kind == "DevicesInfo" && j.asked_for_devices();
            (!asked).then(|| format!("unhandled-{kind}"))
        }
        ("L_KIND", _) => halt("unhandled-other"),
        ("D2_HELD", Body::Held { rule }) => {
            if TOLERATED_HOLDS.contains(&rule.as_str()) {
                if PHONE_HOLDS.contains(&rule.as_str()) {
                    j.tolerate_hold(at_ms);
                }
                None
            } else {
                Some(format!("held-{rule}"))
            }
        }
        ("L_STEP_ASIDE", _) => {
            j.asides += 1;
            j.aside_this_socket = true;
            None
        }
        ("D2_HELD", _) => halt("held-other"),
        ("D1_UNBLOCKED", _) => {
            j.d1_this_socket += 1;
            j.leader = true;
            (j.d1_this_socket > 1).then(|| "leader-burst".to_owned())
        }
        ("E1_CLOSED", Body::Closed { code }) => closed(j, *code),
        ("L_COUNT", Body::Count(c)) => count(j, c, at_ms),
        ("L_COUNT" | "E1_CLOSED", _) => halt("line-unreadable"),
        // RP-b's drop: the outcome is kept for the end and the question (M2); never a halt.
        ("D4_DROP_ACKED", _) => {
            j.drop = Some("removed");
            None
        }
        ("D4_DROP_ALONE", Body::DropAlone { removed }) => {
            j.drop = Some(if *removed { "removed" } else { "not-reached" });
            None
        }
        ("D4_DROP_NO_ANSWER", Body::DropNoAnswer { live: true }) => {
            j.drop.get_or_insert("not-reached");
            None
        }
        ("D4_DROP_WENT", _) => {
            j.drop_went = true;
            None
        }
        _ => None,
    }
}

/// S4 for E1.
fn closed(j: &mut Judge, code: u16) -> Option<String> {
    if j.drop_went && matches!(code, 4113 | 4114) {
        // The drop's own end (RP-b: the ack, or 4113 / 4114 after the ask).
        j.drop = Some("removed");
        return None;
    }
    if code == 4113 && j.removal_asked {
        return None;
    }
    ((4110..=4115).contains(&code) || !NAMED_CLOSES.contains(&code))
        .then(|| format!("close-{code}"))
}

/// S4's close-error, S5a-d and S6 for one count line (a running line is a partial read: judged by
/// the same bounds, each counter counts from the socket's open, never added to another line).
fn count(j: &mut Judge, c: &Count, at_ms: u64) -> Option<String> {
    let n = |f: &str| c.get(f);
    // A tolerated hold in the count (every rule of `held_by` is one, and one of them is a hold the
    // phone resolves: `inbound-known` alone opens no step-aside): remembered before the bounds that
    // read it. Another rule is judged by `judged_counts`.
    if n("held") > 0
        && c.held_by
            .iter()
            .all(|(rule, _)| TOLERATED_HOLDS.contains(&rule.as_str()))
        && c.held_by
            .iter()
            .any(|(rule, _)| PHONE_HOLDS.contains(&rule.as_str()))
    {
        j.tolerate_hold(at_ms);
    }
    let halt = judged_counts(j, c, &n, at_ms);
    if c.end {
        // MEDIUM-2: what this socket took as the leader and wrote as boxes goes into the run's
        // budget (a socket's counters never add up to another's line, but its END does).
        if n("out_unblock") >= 1 || j.leader {
            j.leader_in_total += n("in_csp_message");
        }
        j.job_boxes_total += n("out_box_job");
        // The socket is gone: the next one starts as a follower with no D1, no hold, no step-aside.
        // A step-aside's own end is no loop (S6).
        let stepped_aside = j.aside_this_socket;
        j.d1_this_socket = 0;
        j.leader = false;
        j.phone_hold = false;
        j.aside_this_socket = false;
        if !j.ending && !stepped_aside {
            j.ends.push_back(at_ms);
        }
        while j
            .ends
            .front()
            .is_some_and(|t| at_ms.saturating_sub(*t) > HOUR_MS)
        {
            j.ends.pop_front();
        }
        let recent = j
            .ends
            .iter()
            .filter(|t| at_ms.saturating_sub(**t) <= TEN_MINUTES_MS)
            .count();
        if halt.is_none() && (recent > ENDS_IN_TEN_MINUTES || j.ends.len() > ENDS_IN_AN_HOUR) {
            return Some("socket-loop".to_owned());
        }
    }
    halt
}

fn judged_counts(j: &Judge, c: &Count, n: &dyn Fn(&str) -> u64, at_ms: u64) -> Option<String> {
    let halt = |kind: &str| Some(kind.to_owned());
    if n("out_reflect_transmit") > 0 || n("out_box_transmit") > 0 {
        return halt("transmit");
    }
    if n("held") > 0 {
        // The first rule (the line's order) that is no tolerated one; none left: the hold stands.
        let rule = c
            .held_by
            .iter()
            .map(|(rule, _)| rule.as_str())
            .find(|rule| !TOLERATED_HOLDS.contains(rule));
        if let Some(rule) = rule {
            return Some(format!("held-{rule}"));
        }
    }
    // T-H1's asks: one a minute since the hold stood, and two; every answer follows an ask.
    let asks_at_most = 2 + at_ms.saturating_sub(j.hold_since_ms) / ASK_EVERY_MS;
    let step_aside = j.asked_for_devices() && n("out_other") <= asks_at_most;
    if n("in_other") > 0 && !(step_aside && n("in_other") <= n("out_other")) {
        return halt("unhandled-other");
    }
    if n("in_close_error") > 0 {
        return halt("close-error");
    }
    let in_csp = n("in_csp_message");
    let boxes = n("out_box_job");
    // One row runs at a time per account: a row in flight may have reflected without its boxes.
    let jobs_burst = n("out_reflect_job") > 2 * boxes + 2;
    let drop_unasked = n("out_drop") > 0 && !j.removal_asked;
    let leader = n("out_unblock") >= 1 || j.leader;
    if leader {
        let burst = n("out_unblock") > 1
            || n("out_message_ack") > in_csp
            || n("out_reflect_inbound") > in_csp
            || n("out_ephemeral") > in_csp
            || boxes > 3 * in_csp + 3
            || jobs_burst
            || (n("out_other") > 0 && !step_aside)
            || drop_unasked;
        return burst.then(|| "leader-burst".to_owned());
    }
    if in_csp > 0 {
        return halt("unasked");
    }
    // MEDIUM-2: a follower may finish the jobs earlier leader sockets owed (three boxes per message
    // they took, and three more) and nothing more: every box is a chat server message to a real
    // contact. This socket's own boxes count from its open; the ended sockets' are in the totals.
    let boxes_over_budget = boxes > 0 && j.job_boxes_total + boxes > 3 * j.leader_in_total + 3;
    let wrote = n("out_reflect_inbound") > 0
        || n("out_ephemeral") > 0
        || n("out_message_ack") > 0
        || n("out_tx") > 0
        || n("out_other") > 0
        || jobs_burst
        || boxes_over_budget
        || drop_unasked;
    wrote.then(|| "follower-wrote".to_owned())
}
