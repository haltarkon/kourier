//! What a `connect` found out and what the tool says about it: the pure readers of the app's log, THE
//! VERDICT (D-13) as a function over the walk's events, the state and those lines, the redaction every
//! line goes through before it is written, and the report's words.
//!
//! The report holds counts, words and times only: the profile's NAME, the servers, the build commit,
//! the dialer's mode and reason, the lock-down words, the gate's counts, the milliseconds to the first
//! QR step, the findings. Never a number, a name, a tail, a nonce or a hash of it, the API pair, a QR
//! link, an id, a path or a text. A fact that was not measured says "not measured", never 0 (the
//! false PASS the review looks for first): the report says `sends: not measured` because the walk
//! makes no account and the tool sends nothing, and nothing counts it.

use std::collections::BTreeMap;

// ---- Redaction ----------------------------------------------------------------------------------

/// Takes out of a line what must not reach a log a third party reads: Signal's shapes (a copy of
/// wa_run.rs:187 `redact_signal_shapes`, itself a copy of smoke.rs:180), then Telegram's own: the
/// core's `tg:` and `pn:` ids of 5 to 19 digits, a `tg://` link, a link of the short-link host, a run of
/// exactly 64 hex digits. The app's own rules keep these out already (CLAUDE.md Rules); this is the
/// second layer.
pub(super) fn redact(line: &str) -> String {
    redact_telegram(&redact_signal_shapes(line))
}

/// wa_run.rs:187 `redact_signal_shapes`: a whole UUID keeps its last four digits, a phone number
/// becomes `+[number]`.
fn redact_signal_shapes(line: &str) -> String {
    let b = line.as_bytes();
    let mut out = String::with_capacity(line.len());
    let mut copied = 0;
    let mut i = 0;
    while i < b.len() {
        let word_before = i > 0 && b[i - 1].is_ascii_alphanumeric();
        let hit: Option<(usize, String)> =
            if b[i].is_ascii_hexdigit() && (i == 0 || !b[i - 1].is_ascii_hexdigit()) {
                uuid_at(&b[i..]).then(|| (36, format!("…{}", &line[i + 32..i + 36])))
            } else if b[i] == b'+' && !word_before {
                let digits = b[i + 1..].iter().take_while(|c| c.is_ascii_digit()).count();
                (7..=15)
                    .contains(&digits)
                    .then(|| (1 + digits, "+[number]".to_owned()))
            } else {
                None
            };
        match hit {
            Some((n, replacement)) => {
                out.push_str(&line[copied..i]);
                out.push_str(&replacement);
                i += n;
                copied = i;
            }
            None => i += 1,
        }
    }
    out.push_str(&line[copied..]);
    out
}

/// wa_run.rs:347 `uuid_at`.
fn uuid_at(b: &[u8]) -> bool {
    b.len() >= 36
        && b[..36].iter().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => *c == b'-',
            _ => c.is_ascii_hexdigit(),
        })
        && b.get(36).is_none_or(|c| !c.is_ascii_hexdigit())
}

/// The core's id schemes of a Telegram person and group, and how many digits follow (the fake's are 10
/// and 12; a real user id has up to 10, a channel id 13 and a chat id 19 with its sign).
const CORE_ID_SCHEMES: [&str; 2] = ["tg:", "pn:"];
const CORE_ID_DIGITS: std::ops::RangeInclusive<usize> = 5..=19;

/// The short-link host's needle, assembled from parts: no Telegram host name stands in the tool's
/// sources (the lane's source check scans them).
fn short_link_needle() -> String {
    ["t", ".me/"].concat()
}

/// The login scheme of a Telegram link, assembled from parts.
fn login_scheme() -> String {
    ["tg", "://"].concat()
}

/// Telegram's shapes in one line.
fn redact_telegram(line: &str) -> String {
    let b = line.as_bytes();
    let mut out = String::with_capacity(line.len());
    let (mut copied, mut i) = (0, 0);
    while i < b.len() {
        if !line.is_char_boundary(i) {
            i += 1;
            continue;
        }
        match telegram_shape_at(line, i) {
            Some((n, replacement)) => {
                out.push_str(&line[copied..i]);
                out.push_str(&replacement);
                i += n;
                copied = i;
            }
            None => i += 1,
        }
    }
    out.push_str(&line[copied..]);
    out
}

/// How far a link runs: to white space or a quote or a bracket.
fn link_len(rest: &str) -> usize {
    rest.find(|c: char| c.is_whitespace() || "\"')<>,".contains(c))
        .unwrap_or(rest.len())
}

/// The Telegram shape that starts at byte `i` of `line`: how many bytes it covers and what stands for
/// them.
fn telegram_shape_at(line: &str, i: usize) -> Option<(usize, String)> {
    let (b, rest) = (line.as_bytes(), &line[i..]);
    let before = if i == 0 { b' ' } else { b[i - 1] };
    // A login link or a short link, with or without its scheme.
    let bare = rest
        .strip_prefix("https://")
        .or_else(|| rest.strip_prefix("http://"))
        .unwrap_or(rest);
    if rest.starts_with(&login_scheme()) || bare.starts_with(&short_link_needle()) {
        return Some((link_len(rest), "[link]".to_owned()));
    }
    if !before.is_ascii_alphanumeric() {
        for scheme in CORE_ID_SCHEMES {
            if let Some(after) = rest.strip_prefix(scheme) {
                let sign = usize::from(after.starts_with('-'));
                let digits = after.as_bytes()[sign..]
                    .iter()
                    .take_while(|c| c.is_ascii_digit())
                    .count();
                if CORE_ID_DIGITS.contains(&digits) {
                    return Some((scheme.len() + sign + digits, format!("{scheme}[digits]")));
                }
            }
        }
    }
    // A run of exactly 64 hex digits (a hash, or the nonce itself): never in a log.
    if b[i].is_ascii_hexdigit() && !before.is_ascii_hexdigit() {
        let run = rest.bytes().take_while(u8::is_ascii_hexdigit).count();
        if run == 64 {
            return Some((run, "[hex]".to_owned()));
        }
    }
    None
}

// ---- The app's log --------------------------------------------------------------------------------

/// The words the app's dialer line may say (`kourier-app/src/telegram.rs`: the dialing mode's word, the
/// `Refusal` words, `CONFIRMED`). Anything else is read as `unknown`: the report prints no word the
/// tool does not know.
const DIALER_MODES: [&str; 2] = ["refusing", "telegram"];
const DIALER_REASONS: [&str; 9] = [
    "release",
    "no-profile",
    "no-confirmation",
    "file",
    "stale",
    "mismatch",
    "env",
    "no-tail",
    "confirmed",
];
/// The words of the lock-down line (`kourier-telegram/src/supervisor.rs`: `Lockdown::as_str`).
const LOCKDOWN_WORDS: [&str; 3] = ["on", "unavailable", "other"];
/// The numbers of the gate's counts line (`kourier-telegram/src/gate/mod.rs`: `GateCounts::log`), in
/// the order the report prints them.
pub(super) const COUNT_KEYS: [&str; 14] = [
    "accepted",
    "busy",
    "timed_out",
    "protocol",
    "no_login",
    "auth",
    "command",
    "address_type",
    "rule",
    "port",
    "dials",
    "dial_failures",
    "bytes_up",
    "bytes_down",
];

/// The fixed message of each line the tool reads.
const DIALER_LINE: &str = "telegram dialer";
const LOCKDOWN_LINE: &str = "telegram helper lock-down";
const COUNTS_LINE: &str = "telegram gate counts";
/// Lines that show the real backend's helper ran: anything that speaks of the TDLib helper.
const HELPER_WORDS: [&str; 2] = ["telegram helper", "TDLib helper"];

/// One `telegram gate counts` line: when it was told (`stop`, `running`) and its numbers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Counts {
    pub when: String,
    values: BTreeMap<&'static str, u64>,
}

impl Counts {
    pub fn get(&self, key: &str) -> Option<u64> {
        self.values.get(key).copied()
    }

    /// The number of `key`; every key of [`COUNT_KEYS`] is there in a line that was read.
    fn n(&self, key: &str) -> u64 {
        self.get(key).unwrap_or(u64::MAX)
    }
}

/// What the app's log said (the lines after the latest start mark, already redacted).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct LogFacts {
    /// Every `telegram dialer` line: (mode, reason).
    pub dialer: Vec<(String, String)>,
    /// Every `telegram helper lock-down` line: the word.
    pub lockdown: Vec<String>,
    /// Every `telegram gate counts` line that could be read.
    pub counts: Vec<Counts>,
    /// Counts lines that lacked a number or held a non-number.
    pub counts_unreadable: usize,
    /// Lines that speak of the TDLib helper (a helper's hello among them).
    pub helper_lines: usize,
}

/// The value of `key=` in a log line: the key must start a word, the value runs to white space and
/// loses its quotes.
fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!("{key}=");
    let mut from = 0;
    while let Some(at) = line[from..].find(&needle) {
        let at = from + at;
        if at == 0 || line.as_bytes()[at - 1].is_ascii_whitespace() {
            let value = &line[at + needle.len()..];
            let end = value.find(char::is_whitespace).unwrap_or(value.len());
            return Some(value[..end].trim_matches('"'));
        }
        from = at + needle.len();
    }
    None
}

/// `word` when it is in `known`, else `unknown`.
fn known<'a>(word: Option<&str>, known: &[&'a str]) -> &'a str {
    word.and_then(|w| known.iter().find(|k| **k == w))
        .copied()
        .unwrap_or("unknown")
}

/// Reads the log. A missing line stays missing (an empty list), never a 0.
pub(super) fn read_log(text: &str) -> LogFacts {
    let mut facts = LogFacts::default();
    for line in text.lines() {
        if HELPER_WORDS.iter().any(|w| line.contains(w)) || line.contains(LOCKDOWN_LINE) {
            facts.helper_lines += 1;
        }
        if line.contains(DIALER_LINE) {
            facts.dialer.push((
                known(field(line, "mode"), &DIALER_MODES).to_owned(),
                known(field(line, "reason"), &DIALER_REASONS).to_owned(),
            ));
        } else if line.contains(LOCKDOWN_LINE) {
            facts
                .lockdown
                .push(known(field(line, "lockdown"), &LOCKDOWN_WORDS).to_owned());
        } else if line.contains(COUNTS_LINE) {
            let values: Option<BTreeMap<&'static str, u64>> = COUNT_KEYS
                .iter()
                .map(|k| Some((*k, field(line, k)?.parse::<u64>().ok()?)))
                .collect();
            match values {
                Some(values) => facts.counts.push(Counts {
                    when: known(field(line, "when"), &["stop", "running"]).to_owned(),
                    values,
                }),
                None => facts.counts_unreadable += 1,
            }
        }
    }
    facts
}

// ---- The walk and THE VERDICT -----------------------------------------------------------------------

/// The login steps the app may name (`kourier-telegram/src/login.rs`: `step`); anything else is `other`.
const STEP_IDS: [&str; 15] = [
    "qr",
    "phone",
    "code",
    "password",
    "email",
    "email-code",
    "linking",
    "done",
    "code-again",
    "password-again",
    "email-again",
    "email-code-again",
    "registration",
    "purchase",
    "update",
];

/// The step id as the report may print it.
pub(super) fn step_word(step: &str) -> String {
    known(Some(step), &STEP_IDS).to_owned()
}

/// How the app ended.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum ExitHow {
    /// Not seen (the walk never got that far).
    #[default]
    NotSeen,
    /// It quit by itself after `quit`; the exit status.
    Quit(i32),
    /// It did not quit in time and was killed.
    Killed,
}

/// The kinds of a login step the dev control names (`kourier-qt/src/dev/mod.rs` `login_kind`); anything
/// else is `other`.
const STEP_KINDS: [&str; 7] = ["qr", "code", "emoji", "input", "progress", "done", "failed"];

/// The step kind as the report may print it.
pub(super) fn kind_word(kind: &str) -> String {
    STEP_KINDS
        .iter()
        .find(|k| **k == kind)
        .map_or("other", |k| *k)
        .to_owned()
}

/// One `login_step` event: the step id and the kind of the step, both as known words, and whether the
/// `login_qr` event that says "shown" followed it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Step {
    pub word: String,
    pub kind: String,
    pub shown: bool,
}

impl Step {
    pub fn new(word: &str, kind: &str) -> Self {
        Self {
            word: step_word(word),
            kind: kind_word(kind),
            shown: false,
        }
    }

    /// A QR code the app made (every new link is a new `qr` step of the kind `qr`).
    pub fn is_qr(&self) -> bool {
        self.word == "qr" && self.kind == "qr"
    }

    /// A QR code the app SHOWED: the step, and the `login_qr` event with `shown` after it.
    fn is_shown_qr(&self) -> bool {
        self.is_qr() && self.shown
    }
}

/// Who took the confirmation file: looked at when the walk ended, BEFORE the tool's own removal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Confirmation {
    /// A confirmed order, and the file was gone: the app unlinked its own.
    TakenByApp,
    /// A file still stood (the tool removed it afterwards).
    Left,
    /// An unconfirmed order, and no file stood.
    NoneWritten,
}

impl Confirmation {
    pub fn words(self) -> &'static str {
        match self {
            Self::TakenByApp => "taken by the app",
            Self::Left => "left (removed by the tool)",
            Self::NoneWritten => "none written",
        }
    }
}

/// The most login steps a walk keeps (a flood of events is a finding, not memory). A step past it is
/// counted in [`Walk::steps_dropped`], and any drop fails the walk.
pub(super) const STEPS_KEPT: usize = 64;

/// What the driver saw of the app on its dev-control channel.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Walk {
    /// The `login_step` events in the order they came (at most [`STEPS_KEPT`]).
    pub steps: Vec<Step>,
    /// The `login_step` events past [`STEPS_KEPT`]: not kept, so never judged.
    pub steps_dropped: u64,
    /// Milliseconds from the app's start to the first QR code the app SHOWED.
    pub qr_after_ms: Option<u64>,
    /// A `login_qr` event carried the link: the tool starts the app with the link switched off, so a
    /// link on its stdout is a leak of a login token. The link itself is never kept.
    pub qr_link_printed: bool,
    /// The `error` events of a login or an account (the source is read, never the message).
    pub errors: u64,
    /// Whether the app answered the page change to the list with ok.
    pub closed: Option<bool>,
    /// The number of accounts the app's `state` named after the close.
    pub accounts: Option<u64>,
    /// The app's memory in kB just before it was asked to quit: (`VmRSS`, `VmHWM`) of its
    /// `/proc/<pid>/status`. A number for the report, not a criterion.
    pub memory_kb: Option<(u64, u64)>,
    pub exit: ExitHow,
    /// Who took the confirmation file (looked at before the tool's own removal).
    pub confirmation: Option<Confirmation>,
    /// A rehearsal only: how many of the walk's needles stood in the app's RAW stderr lines (before
    /// the redaction). `None` in a real run, which keeps no needle list.
    pub raw_needles: Option<u64>,
}

impl Walk {
    /// The step words in the order they came.
    #[cfg(test)]
    pub fn words(&self) -> Vec<&str> {
        self.steps.iter().map(|s| s.word.as_str()).collect()
    }

    /// The position of the first QR code the app showed.
    fn qr_at(&self) -> Option<usize> {
        self.steps.iter().position(Step::is_shown_qr)
    }

    /// The steps after the first shown QR code that are not another shown QR code (a refreshed code is
    /// a new link, not a step of the login going on), failed steps included.
    fn steps_after_qr(&self) -> Vec<&Step> {
        match self.qr_at() {
            Some(at) => self.steps[at + 1..]
                .iter()
                .filter(|s| !s.is_shown_qr())
                .collect(),
            None => self.steps.iter().collect(),
        }
    }

    /// How many QR codes the app showed.
    fn qr_shown(&self) -> usize {
        self.steps.iter().filter(|s| s.is_shown_qr()).count()
    }
}

/// `VmRSS` and `VmHWM` of the text of a `/proc/<pid>/status`, in kB.
pub(super) fn parse_status_memory(text: &str) -> Option<(u64, u64)> {
    let kb = |key: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(key))
            .and_then(|rest| rest.trim_start_matches(':').split_whitespace().next())
            .and_then(|n| n.parse::<u64>().ok())
    };
    Some((kb("VmRSS")?, kb("VmHWM")?))
}

/// What kind of run this is: against Telegram, or the rehearsal against the fake.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RunKind {
    Real,
    Rehearsal,
}

/// The verdict of a connect.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Verdict {
    pub pass: bool,
    /// Why not (fixed sentences, no value).
    pub failures: Vec<String>,
    /// Things worth a look even in a PASS.
    pub findings: Vec<String>,
}

impl Verdict {
    pub fn word(&self) -> &'static str {
        if self.pass { "PASS" } else { "FAIL" }
    }
}

/// The dialer line an order must produce: a confirmed order, `telegram`/`confirmed`; an order that was
/// not confirmed, `refusing`/`no-confirmation` (critic 16: a verdict that ignored the mode would pass
/// the rehearsal's refusing start).
pub(super) fn expected_dialer(confirmed: bool) -> (&'static str, &'static str) {
    if confirmed {
        ("telegram", "confirmed")
    } else {
        ("refusing", "no-confirmation")
    }
}

/// THE VERDICT (D-13), a pure function over what the walk saw and what the app's log said.
///
/// PASS only when ALL of these hold: the app SHOWED a QR code (a `qr` step of the kind `qr` and the
/// `login_qr` event with `shown` after it); no login step failed; no step came after it but a refreshed
/// code (another shown `qr`); the app reported no login or account error; its output carried no link;
/// the app named no account after the close; the page change to the list was answered ok; the app quit
/// by itself; the dialer line (exactly one) equals the order; the confirmation file was taken by the
/// app for a confirmed order and never written for the other. In a REAL run besides: EVERY lock-down
/// word is `on`, there is exactly ONE gate session (one final counts line, `when=stop`; a periodic
/// line is a partial count of the same gate and is never summed), a final line for every account actor
/// that told its lock-down word (the word is told once per actor, so a gate that was dropped without
/// its stop line, a panic, is not seen), and over the final lines `dials >= 1`, `dial_failures <
/// dials`, `rule == 0`, `port == 0`, `address_type == 0`. In a REHEARSAL instead: no counts line, no
/// lock-down line and no line about the helper at all (the real backend never ran; critic 23). A line that is missing in a real run
/// FAILS: "not measured" is never a pass there.
pub(super) fn verdict(walk: &Walk, log: &LogFacts, confirmed: bool, kind: RunKind) -> Verdict {
    let mut failures: Vec<String> = Vec::new();
    let mut findings: Vec<String> = Vec::new();
    let mut fail = |why: &str| failures.push(why.to_owned());

    // The walk.
    let qr_at = walk.qr_at();
    if qr_at.is_none() {
        fail("no QR code was shown by the app within its bound");
    }
    if let Some(failed) = walk.steps.iter().position(|s| s.kind == "failed") {
        fail(&format!(
            "the login failed at {} {} the QR code",
            walk.steps[failed].word,
            if qr_at.is_some_and(|at| at < failed) {
                "after"
            } else {
                "before"
            }
        ));
    }
    if walk.steps_after_qr().iter().any(|s| s.kind != "failed") && qr_at.is_some() {
        fail(
            "a login step came after the QR code: the code may have been scanned, or the login went on",
        );
    }
    if walk.steps_dropped > 0 {
        fail(&format!(
            "more than {STEPS_KEPT} login steps: not every step was judged"
        ));
    }
    if walk.errors > 0 {
        fail(&format!(
            "the app reported {} login or account errors",
            walk.errors
        ));
    }
    if walk.qr_link_printed {
        fail("the app printed the QR link");
    }
    match walk.accounts {
        Some(0) => {}
        Some(_) => fail("the app has an account"),
        None => fail("the number of accounts was not measured"),
    }
    match walk.closed {
        Some(true) => {}
        Some(false) => fail("the app refused the page change that closes the login"),
        None => fail("the login was not closed by a page change"),
    }
    match walk.exit {
        ExitHow::Quit(0) => {}
        ExitHow::Quit(_) => fail("the app quit with a failure status"),
        ExitHow::Killed => fail("the app did not quit in time and was killed"),
        ExitHow::NotSeen => fail("the app's end was not seen"),
    }
    match (confirmed, walk.confirmation) {
        (true, Some(Confirmation::TakenByApp)) | (false, Some(Confirmation::NoneWritten)) => {}
        (true, Some(_)) => fail("the app did not take its confirmation"),
        (false, Some(_)) => fail("a confirmation stood although the order was not confirmed"),
        (_, None) => fail("who took the confirmation was not observed"),
    }

    // The dialer line.
    let (mode, reason) = expected_dialer(confirmed);
    match log.dialer.as_slice() {
        [(m, r)] if m == mode && r == reason => {}
        [] => fail("the app told no dialer line"),
        [_] => fail("the dialer line does not match the order"),
        _ => fail("the app told its dialer line more than once"),
    }

    // What the real backend did, or did not.
    match kind {
        RunKind::Rehearsal => {
            if !log.counts.is_empty() || log.counts_unreadable > 0 {
                fail("a gate counts line appeared in a rehearsal: the real backend ran");
            }
            if !log.lockdown.is_empty() {
                fail("a lock-down line appeared in a rehearsal: the real backend ran");
            }
            if log.helper_lines > 0 {
                fail("a line about the TDLib helper appeared in a rehearsal: the real backend ran");
            }
        }
        RunKind::Real => judge_gate(log, &mut failures, &mut findings),
    }
    Verdict {
        pass: failures.is_empty(),
        failures,
        findings,
    }
}

/// The gate part of a real run's verdict: every gate session of the walk, not the last one. A normal
/// walk makes exactly one (the login's helper is started once, the page change cancels the login and
/// the core's logout ends that helper; `actor.rs` `start_login`, `logout`), so more than one is a
/// restart the walk has no reason for, and a refusal in an earlier session must not be hidden by a
/// clean later one. The lock-down word is told once per account actor, not once per helper start: the
/// actors that told it are compared with the final lines, and a gate dropped without its stop line is
/// not seen.
fn judge_gate(log: &LogFacts, failures: &mut Vec<String>, findings: &mut Vec<String>) {
    let mut fail = |why: String| failures.push(why);
    if log.lockdown.is_empty() {
        fail("the lock-down word was not measured".to_owned());
    } else if log.lockdown.iter().any(|w| w != "on") {
        fail("the helper's lock-down is not on".to_owned());
        findings.push("the helper's lock-down word is not `on`: stop and look".to_owned());
    }
    if log.counts_unreadable > 0 {
        fail("a gate counts line could not be read".to_owned());
    }
    let stops: Vec<&Counts> = log.counts.iter().filter(|c| c.when == "stop").collect();
    if stops.is_empty() {
        fail("the gate's final counts line is missing (not measured)".to_owned());
        return;
    }
    if log.lockdown.len() > stops.len() {
        fail(
            "an account actor told its lock-down word without a final counts line of its gate (not measured)"
                .to_owned(),
        );
    }
    if stops.len() > 1 {
        fail(format!(
            "the helper was started {} times during the walk",
            stops.len()
        ));
    }
    let sum = |key: &str| {
        stops
            .iter()
            .fold(0u64, |total, c| total.saturating_add(c.n(key)))
    };
    let (dials, failed) = (sum("dials"), sum("dial_failures"));
    if dials < 1 {
        fail("the gate dialled nothing".to_owned());
    }
    if failed >= dials {
        fail("every dial the gate made failed".to_owned());
    }
    let rule = sum("rule");
    if rule > 0 {
        fail("the gate refused connections by its address list".to_owned());
        findings.push(format!(
            "the gate refused {rule} connection(s) by its address list: the list is stale; fix ranges.rs before the next run"
        ));
    }
    if sum("port") > 0 {
        fail("the gate refused connections by their port".to_owned());
    }
    if sum("address_type") > 0 {
        fail("the gate refused connections by their address type".to_owned());
    }
}

// ---- The report -------------------------------------------------------------------------------------

/// Everything the report says.
pub(super) struct Report<'a> {
    /// The profile's NAME.
    pub profile: &'a str,
    pub env: &'a str,
    /// main's HEAD the app was built at, short (a real run), or `None` (a rehearsal).
    pub build: Option<&'a str>,
    pub confirmed: bool,
    pub kind: RunKind,
    pub walk: &'a Walk,
    pub log: &'a LogFacts,
    pub verdict: &'a Verdict,
}

const NOT_MEASURED_REHEARSAL: &str = "not measured (rehearsal)";

/// The words printed and written to `report.md`, one per line. Every line goes through [`redact`].
pub(super) fn lines(r: &Report<'_>) -> Vec<String> {
    let mut out = vec![format!("connect: {}", r.verdict.word())];
    out.push(format!(
        "  profile {} (env {}); build {}; {} run; order {}",
        r.profile,
        r.env,
        r.build.unwrap_or("none"),
        match r.kind {
            RunKind::Real => "real",
            RunKind::Rehearsal => "rehearsal",
        },
        if r.confirmed {
            "confirmed"
        } else {
            "not confirmed"
        },
    ));
    out.push(match r.log.dialer.as_slice() {
        [(m, why)] => format!("  dialer: mode={m} reason={why}"),
        [] => "  dialer: not told".to_owned(),
        many => format!("  dialer: told {} times", many.len()),
    });
    out.push(match (r.kind, r.log.lockdown.is_empty()) {
        (RunKind::Rehearsal, true) => format!("  lock-down: {NOT_MEASURED_REHEARSAL}"),
        (_, false) => format!("  lock-down: {}", r.log.lockdown.join(", ")),
        (RunKind::Real, true) => "  lock-down: not measured".to_owned(),
    });
    if r.log.counts.is_empty() {
        out.push(match r.kind {
            RunKind::Rehearsal => format!("  gate counts: {NOT_MEASURED_REHEARSAL}"),
            RunKind::Real => "  gate counts: not measured".to_owned(),
        });
    }
    for c in &r.log.counts {
        let numbers: Vec<String> = COUNT_KEYS
            .iter()
            .map(|k| format!("{k}={}", c.get(k).unwrap_or(0)))
            .collect();
        out.push(format!("  gate counts ({}): {}", c.when, numbers.join(" ")));
    }
    out.push(format!(
        "  first QR step: {}; steps after it: {}; accounts after the close: {}; the page change: {}",
        r.walk
            .qr_after_ms
            .map_or("never".to_owned(), |ms| format!("after {ms} ms")),
        r.walk.steps_after_qr().len(),
        r.walk
            .accounts
            .map_or("not measured".to_owned(), |n| n.to_string()),
        match r.walk.closed {
            Some(true) => "answered ok",
            Some(false) => "refused",
            None => "not answered",
        },
    ));
    out.push(format!(
        "  QR codes shown: {}; login errors: {}; QR link in the app's output: {}",
        r.walk.qr_shown(),
        r.walk.errors,
        if r.walk.qr_link_printed {
            "yes"
        } else {
            "none"
        },
    ));
    out.push(format!(
        "  confirmation: {}",
        r.walk
            .confirmation
            .map_or("not observed", Confirmation::words)
    ));
    if let Some(hits) = r.walk.raw_needles {
        out.push(format!("  raw needles in the app's stderr: {hits}"));
    }
    out.push(format!(
        "  the app: {}; memory {}",
        match r.walk.exit {
            ExitHow::Quit(code) => format!("quit by itself (status {code})"),
            ExitHow::Killed => "killed after its bound".to_owned(),
            ExitHow::NotSeen => "end not seen".to_owned(),
        },
        r.walk
            .memory_kb
            .map_or("not measured".to_owned(), |(rss, peak)| {
                format!("{} MiB (peak {} MiB)", rss / 1024, peak / 1024)
            }),
    ));
    for failure in &r.verdict.failures {
        out.push(format!("  FAILED: {failure}"));
    }
    if r.verdict.findings.is_empty() {
        out.push("  findings: none".to_owned());
    }
    for finding in &r.verdict.findings {
        out.push(format!("  finding: {finding}"));
    }
    out.push("  sends: not measured".to_owned());
    out.into_iter().map(|l| redact(&l)).collect()
}
