//! Every line TR reads from the app's stderr (T5): ONE table, the words and shapes of TH6's line
//! contract (`target/threema/th6-lines.md`; the source of truth is
//! `crates/kourier-threema/src/conn/lines.rs`, one `pub(crate) const` per line), RP-b's drop lines
//! (D4) and E6 in `kourier-app`. Check 11 of the start ([`super::preflight::check_lines`]), the
//! supervisor's reader ([`super::supervise`]) and the judge ([`super::judge`]) read THIS table; a
//! twin test (`tests/lines.rs`) reads the backend's file and finds every const there in its exact
//! words, every const of that file here, the count line's fields in `Frames::log`'s order and the
//! close codes the spec names in `close.rs`, so a word TH6 changes fails the same day.
//!
//! A line on the app's stderr is tracing's plain format: the time, the level, the target (spans
//! before it when there are any), `: `, the MESSAGE, then each field as ` name=value` in the call's
//! order. A number or a word is bare, a `&str` field of the old lines is in double quotes
//! (`meaning="Normal closure"`). THE PARSER keeps counts and names only: numbers, words of the
//! protocol (letters, digits, `_`, `-`), and the count line's `held_by` rules; a quoted value is
//! never kept (its words are fixed, but nothing needs them).

use std::collections::BTreeMap;

/// A level of tracing's plain format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Level {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl Level {
    fn of(word: &str) -> Option<Self> {
        Some(match word {
            "TRACE" => Self::Trace,
            "DEBUG" => Self::Debug,
            "INFO" => Self::Info,
            "WARN" => Self::Warn,
            "ERROR" => Self::Error,
            _ => return None,
        })
    }
}

/// One line the tool reads from the app's log, and where HEAD's source must say it.
pub(super) struct LineRow {
    /// The repository path of the file that holds it.
    pub file: &'static str,
    /// The `const` that holds the words, when the line is one (`None`: the words stand in a log
    /// call as a literal and nothing else names them).
    pub name: Option<&'static str>,
    /// The words, as the app logs them (the message before its `name=value` fields).
    pub text: &'static str,
    /// The level the call prints it at (the tests pin it; the tool itself matches the words).
    #[cfg_attr(not(test), allow(dead_code))]
    pub level: Level,
    /// Its fields, in the call's order (the count line's are [`COUNT_FIELDS`]; a line with two
    /// shapes names the live socket's, `th6-lines.md` says the other; the tests pin them).
    #[cfg_attr(not(test), allow(dead_code))]
    pub fields: &'static [&'static str],
}

/// The backend's file of the contract.
pub(super) const CONN_LINES: &str = "crates/kourier-threema/src/conn/lines.rs";
/// The app's file that holds E6 as a literal.
pub(super) const APP_THREEMA: &str = "crates/kourier-app/src/threema.rs";
/// The backend's table of close codes and the spec's words for them (`close::sentence`); the twin
/// test reads it.
#[cfg(test)]
pub(super) const CLOSE_FILE: &str = "crates/kourier-threema/src/close.rs";

// ---- The words (th6-lines.md "The new lines", "The existing lines") ----------------------------------

/// L-COUNT (INFO): one socket's frame counts, `when=running` every 60 s of the actor's clock while
/// the socket stands (a partial read: never added to another line), `when=end` once at its end
/// (whatever ended it; none for a socket that never opened or a killed process: "not measured").
pub(super) const L_COUNT: &str = "socket frame counts";
/// L-KIND (INFO): a payload Kourier does not handle, once per (server, kind) and socket;
/// `server=<chat|mediator>` `kind=<libthreema's variant name>`.
pub(super) const L_KIND: &str = "a server payload Kourier does not handle";
/// L-UNASKED (WARN): THE H1 GUARD fired (a chat server message on a socket that may not take one);
/// the socket closes 1000 right after it. No fields.
pub(super) const L_UNASKED: &str = "a chat server message came unasked; closing";
/// D1 (INFO): the leader socket wrote its one unblock (not printed before TH6 L4b). No fields.
pub(super) const D1_UNBLOCKED: &str = "incoming messages were unblocked as the leader";
/// D2 (INFO): a chat server message held for the phone, once per message; `rule=<rule>` (the count
/// line's `held_by` key; not printed before TH6 L4b).
pub(super) const D2_HELD: &str = "a chat server message is held for the phone";
/// D4 (INFO, RP-b): the live socket got the ack of the own device's drop. No fields.
pub(super) const D4_DROP_ACKED: &str = "the mediator acked the own device's drop";
/// D4 (INFO, RP-b): the drop's wait ran out; the live socket's has no fields (final: no second
/// socket follows), the D2M-only socket's has `written=<bool>` (its outcome line follows).
pub(super) const D4_DROP_NO_ANSWER: &str = "the mediator did not answer the drop in time";
/// D4 (INFO, RP-b): the live socket wrote `DropDevice`. No fields.
pub(super) const D4_DROP_WENT: &str = "the own device's drop went on the live socket";
/// D4 (INFO, RP-b S-5): the live socket's write of `DropDevice` failed or stalled. No fields.
pub(super) const D4_DROP_NOT_WRITTEN: &str =
    "the own device's drop could not be written to the live socket";
/// D4 (INFO, RP-b): the ask came before `ServerInfo` or on a socket that was going; the actor
/// drops on a D2M-only socket. No fields.
pub(super) const D4_DROP_NO_SOCKET: &str = "no socket to drop the own device on: closed";
/// D4 (INFO, RP-b): the D2M-only socket's outcome, `outcome=<Removed|NotReached>` (once per logout
/// that needed it; the drop of a join that failed after its ClientHello ends here).
pub(super) const D4_DROP_ALONE: &str = "the own device's drop on a socket of its own";
/// E1 (INFO): the server closed the live socket; `code=<n>` `meaning="<the spec's words>"`.
pub(super) const E1_CLOSED: &str = "the mediator closed the connection";
/// E2 (WARN): the clock rule closed the socket; `server="mediator"|"chat server"` `minutes=<n>`.
pub(super) const E2_CLOCK: &str = "the clock is off by more than 20 minutes";
/// E3 (WARN): a TLS pin refusal (then held, `UpdateApp`); `kind=<Debug of the refusal>`.
pub(super) const E3_UNTRUSTED: &str = "the server's certificate is not one Kourier trusts";
/// E4 (WARN): libthreema refused a server message; the live socket's `what="<..>"`
/// `unsupported=<bool>`, the D2M-only socket's `what="d2m"` `code=<n>`; both then (RT1 attempt 1)
/// `kind=<the error's variant as a word>`, `name=<libthreema's message name>` where it has one and
/// `payload_type=<n>` for `unexpected-payload-type` (`th6-lines.md`, "After RT1 attempt 1").
pub(super) const E4_REFUSED: &str = "the server's message was refused";
/// E5 (ERROR): the connection task panicked. No fields.
pub(super) const E5_PANICKED: &str = "the connection task panicked";
/// E7 (WARN): a join's first `ServerInfo` was not NEW. No fields.
pub(super) const E7_NOT_NEW: &str = "the mediator did not register the joined device as new";
/// E8 (WARN): the core refused zone 2's first row. No fields.
pub(super) const E8_FIRST_ROW: &str =
    "the core refused the joined account's first row; the link ends";
/// E6 (WARN, `kourier-app`): the keeper's list of test IDs was refused (`problem=<word>`).
pub(super) const E6_LIST_NOT_USED: &str = "the list of Threema test IDs is not used";
/// The row id of E6 (it has no const in the source: a literal in the app's log call).
pub(super) const ROW_E6: &str = "E6_LIST_NOT_USED";

/// L_STEP_ASIDE (INFO, T-H1; a const of `conn/lines.rs` since TH6 FIX-B): the leader socket held a message
/// only the phone resolves and the mediator showed another device online, so it closes 1000 ONCE
/// (`Outcome::Yield`, no failed run, at most once every 15 minutes per actor) and the phone can lead;
/// `held=<n>`. Its `GetDevicesInfo` ask counts as `out_other` and the answer as `in_other` with the
/// L-KIND line `server=mediator kind=DevicesInfo` (the judge reads them as the step-aside's own).
pub(super) const L_STEP_ASIDE: &str =
    "messages are held for the phone and another device is online; stepping aside so it can lead";

/// The count line's fields, in `Frames::log`'s order: `when`, the twelve read counters, the
/// fourteen written counters, `held`, `held_by`. Every field is always present, zeros included.
pub(super) const COUNT_FIELDS: [&str; 29] = [
    "when",
    "in_reflected",
    "in_reflect_ack",
    "in_dry",
    "in_promoted",
    "in_drop_ack",
    "in_tx",
    "in_csp_message",
    "in_message_ack",
    "in_queue_complete",
    "in_echo",
    "in_close_error",
    "in_other",
    "out_reflected_ack",
    "out_reflect_job",
    "out_reflect_transmit",
    "out_reflect_inbound",
    "out_ephemeral",
    "out_box_job",
    "out_box_transmit",
    "out_message_ack",
    "out_unblock",
    "out_tx",
    "out_echo",
    "out_idle",
    "out_drop",
    "out_other",
    "held",
    "held_by",
];

/// The close codes whose words the spec gives (`close::sentence`): any other code of E1 is "a code
/// the spec does not name" (stop rule S4). The twin test reads `close.rs` for them.
pub(super) const NAMED_CLOSES: [u16; 16] = [
    1000, 1001, 1011, 4000, 4001, 4009, 4010, 4011, 4012, 4013, 4110, 4111, 4112, 4113, 4114, 4115,
];

/// THE TABLE: every line TR reads (the nineteen consts of the backend's contract, then E6). Check
/// 11 refuses a HEAD that lacks one; the judge reads them by `name`.
pub(super) const LINES: &[LineRow] = &[
    row("L_COUNT", L_COUNT, Level::Info, &COUNT_FIELDS),
    row("L_KIND", L_KIND, Level::Info, &["server", "kind"]),
    row("L_UNASKED", L_UNASKED, Level::Warn, &[]),
    row("D1_UNBLOCKED", D1_UNBLOCKED, Level::Info, &[]),
    row("D2_HELD", D2_HELD, Level::Info, &["rule"]),
    row("D4_DROP_ACKED", D4_DROP_ACKED, Level::Info, &[]),
    row("D4_DROP_NO_ANSWER", D4_DROP_NO_ANSWER, Level::Info, &[]),
    row("D4_DROP_WENT", D4_DROP_WENT, Level::Info, &[]),
    row("D4_DROP_NOT_WRITTEN", D4_DROP_NOT_WRITTEN, Level::Info, &[]),
    row("D4_DROP_NO_SOCKET", D4_DROP_NO_SOCKET, Level::Info, &[]),
    row("D4_DROP_ALONE", D4_DROP_ALONE, Level::Info, &["outcome"]),
    row("E1_CLOSED", E1_CLOSED, Level::Info, &["code", "meaning"]),
    row("E2_CLOCK", E2_CLOCK, Level::Warn, &["server", "minutes"]),
    row("E3_UNTRUSTED", E3_UNTRUSTED, Level::Warn, &["kind"]),
    row(
        "E4_REFUSED",
        E4_REFUSED,
        Level::Warn,
        &["what", "unsupported", "kind", "name", "payload_type"],
    ),
    row("E5_PANICKED", E5_PANICKED, Level::Error, &[]),
    row("E7_NOT_NEW", E7_NOT_NEW, Level::Warn, &[]),
    row("E8_FIRST_ROW", E8_FIRST_ROW, Level::Warn, &[]),
    row("L_STEP_ASIDE", L_STEP_ASIDE, Level::Info, &["held"]),
    // E6 is the app's own WARN when the list of test IDs was refused; it is a literal in a log
    // call, not a const.
    LineRow {
        file: APP_THREEMA,
        name: None,
        text: E6_LIST_NOT_USED,
        level: Level::Warn,
        fields: &["problem"],
    },
];

const fn row(
    name: &'static str,
    text: &'static str,
    level: Level,
    fields: &'static [&'static str],
) -> LineRow {
    LineRow {
        file: CONN_LINES,
        name: Some(name),
        text,
        level,
        fields,
    }
}

// ---- The parser -----------------------------------------------------------------------------------------

/// The row id of a table row: the const's name, or the id of a literal's row.
pub(super) fn row_id(row: &LineRow) -> &'static str {
    row.name.unwrap_or(ROW_E6)
}

/// The row a collected line has when it is none of the table's: rule S8's two belts.
pub(super) const ROW_ERROR: &str = "ERROR_LINE";
pub(super) const ROW_PANIC: &str = "PANIC";

/// The table row (or belt) a raw stderr line of the app is, if any. The words are matched anywhere in
/// the line (tracing prints the time, the level and the target before the message).
pub(super) fn row_of(line: &str) -> Option<&'static str> {
    if let Some(row) = LINES.iter().find(|row| line.contains(row.text)) {
        return Some(row_id(row));
    }
    if line.contains("panicked at") {
        return Some(ROW_PANIC);
    }
    // tracing's level column: ` ERROR ` between the time and the target.
    if line.contains(" ERROR ") {
        return Some(ROW_ERROR);
    }
    None
}

/// One frame-count line, read whole.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Count {
    /// `when=end`: the socket ended (else `running`: a partial read).
    pub end: bool,
    /// The 26 counters by field name, and `held`.
    pub n: BTreeMap<&'static str, u64>,
    /// `held_by`: the rules and their counts, in the line's order (`none` = empty).
    pub held_by: Vec<(String, u64)>,
}

impl Count {
    /// A counter (0 for a name the line does not have: the parser refuses such a line first).
    pub fn get(&self, field: &str) -> u64 {
        self.n.get(field).copied().unwrap_or(0)
    }
}

/// What a line said, in counts and names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Body {
    /// L-COUNT.
    Count(Count),
    /// L-KIND: the server and the kind words.
    Kind { server: String, kind: String },
    /// D2: the rule word.
    Held { rule: String },
    /// T-H1's step-aside line: how many messages the socket held for the phone (`held=<n>`, 0 when
    /// the field is missing).
    Aside { held: u64 },
    /// E1: the code.
    Closed { code: u16 },
    /// D4's outcome on the D2M-only socket: removed (`Removed`) or not.
    DropAlone { removed: bool },
    /// D4's "did not answer in time": `live` when it has no `written` field (the live socket's,
    /// final), else the D2M-only socket's (its outcome line follows).
    DropNoAnswer { live: bool },
    /// A line of the table whose fields the tool does not read.
    Plain,
    /// Rule S8's belts: an ERROR line (the first segment of its target, a word, when it has one)
    /// or a panic.
    Belt { error_of: Option<String> },
    /// A line of the table whose fields are not the contract's: the tool cannot read it (a blind
    /// judge is no run).
    Unreadable,
}

/// One stderr line of the app that the tool reads, parsed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Parsed {
    /// The table's row id (the const's name, [`ROW_E6`]) or a belt ([`ROW_ERROR`], [`ROW_PANIC`]).
    pub row: &'static str,
    /// The line's level, when it has tracing's level column.
    pub level: Option<Level>,
    pub body: Body,
}

/// A field's value as the parser keeps it.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Val {
    Num(u64),
    Word(String),
    /// A quoted value or a value that is no word: its text is not kept.
    Other,
}

/// A protocol word: 1 to 40 of letters, digits, `_`, `-`.
fn is_word(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 40
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// The `name=value` fields after the message (`rest`), in order. `None` when the rest is not fields
/// (a name that is no word, an unclosed quote).
fn fields_of(rest: &str) -> Option<Vec<(String, Val, String)>> {
    let mut out = Vec::new();
    let mut s = rest.trim_start();
    while !s.is_empty() {
        let eq = s.find('=')?;
        let name = &s[..eq];
        if !is_word(name) {
            return None;
        }
        s = &s[eq + 1..];
        let (raw, val, after) = if let Some(quoted) = s.strip_prefix('"') {
            // A quoted value ends at the next quote that is not escaped.
            let mut end = None;
            let mut escaped = false;
            for (i, c) in quoted.char_indices() {
                match c {
                    '\\' if !escaped => escaped = true,
                    '"' if !escaped => {
                        end = Some(i);
                        break;
                    }
                    _ => escaped = false,
                }
            }
            let end = end?;
            (String::new(), Val::Other, &quoted[end + 1..])
        } else {
            let end = s.find(' ').unwrap_or(s.len());
            let token = &s[..end];
            let val = match token.parse::<u64>() {
                Ok(n) => Val::Num(n),
                Err(_) if is_word(token) => Val::Word(token.to_owned()),
                Err(_) => Val::Other,
            };
            (token.to_owned(), val, &s[end..])
        };
        out.push((name.to_owned(), val, raw));
        s = after.trim_start();
    }
    Some(out)
}

/// `held_by`: `none`, or `<rule>:<n>` joined by `,` (rules are words). `None` when it is neither.
fn held_by_of(raw: &str) -> Option<Vec<(String, u64)>> {
    if raw == "none" {
        return Some(Vec::new());
    }
    raw.split(',')
        .map(|pair| {
            let (rule, n) = pair.split_once(':')?;
            if !is_word(rule) {
                return None;
            }
            Some((rule.to_owned(), n.parse::<u64>().ok()?))
        })
        .collect()
}

/// The count line's fields, strictly: exactly [`COUNT_FIELDS`] in order, `when` a known word, every
/// counter a number, `held` the sum of `held_by`.
fn count_of(fields: &[(String, Val, String)]) -> Option<Count> {
    if fields.len() != COUNT_FIELDS.len()
        || fields
            .iter()
            .zip(COUNT_FIELDS)
            .any(|((name, _, _), want)| name != want)
    {
        return None;
    }
    let end = match &fields[0].1 {
        Val::Word(w) if w == "end" => true,
        Val::Word(w) if w == "running" => false,
        _ => return None,
    };
    let mut n = BTreeMap::new();
    for ((_, val, _), name) in fields.iter().zip(COUNT_FIELDS).skip(1).take(27) {
        let Val::Num(v) = val else { return None };
        n.insert(name, *v);
    }
    let held_by = held_by_of(&fields[28].2)?;
    let sum: u64 = held_by.iter().map(|(_, k)| k).sum();
    if n.get("held") != Some(&sum) {
        return None;
    }
    Some(Count { end, n, held_by })
}

fn field<'a>(fields: &'a [(String, Val, String)], name: &str) -> Option<&'a Val> {
    fields.iter().find(|(n, _, _)| n == name).map(|(_, v, _)| v)
}

fn word_field(fields: &[(String, Val, String)], name: &str) -> Option<String> {
    match field(fields, name)? {
        Val::Word(w) => Some(w.clone()),
        _ => None,
    }
}

/// The level column and the target's first segment (`kourier_core` of `kourier_core::x:`) before
/// `at`, the message's place in the line. Spans (`name{..}:`) may stand between them.
fn level_and_crate(head: &str) -> (Option<Level>, Option<String>) {
    let mut tokens = head.split_whitespace();
    let level = tokens.by_ref().find_map(Level::of);
    let target = level.and_then(|_| {
        tokens
            .filter_map(|t| t.strip_suffix(':'))
            .filter(|t| !t.contains('{'))
            .map(|t| t.split("::").next().unwrap_or(t))
            .find(|t| is_word(t))
            .map(str::to_owned)
    });
    (level, target)
}

/// Parses one raw stderr line of the app: `None` when it is none of the table's lines and no belt.
/// Counts and names only leave this function (a quoted value or a value that is no word is dropped).
pub(super) fn parse(line: &str) -> Option<Parsed> {
    let row = row_of(line)?;
    let found = LINES
        .iter()
        .find(|r| row_id(r) == row)
        .and_then(|r| line.find(r.text).map(|at| (r, at)));
    let Some((r, at)) = found else {
        // A belt: an ERROR line or a panic.
        let (level, target) = level_and_crate(line);
        let error_of = if row == ROW_ERROR { target } else { None };
        return Some(Parsed {
            row,
            level,
            body: Body::Belt { error_of },
        });
    };
    let (level, _) = level_and_crate(&line[..at]);
    let rest = &line[at + r.text.len()..];
    let Some(fields) = fields_of(rest) else {
        return Some(Parsed {
            row,
            level,
            body: Body::Unreadable,
        });
    };
    let body = match row {
        "L_COUNT" => count_of(&fields).map(Body::Count),
        "L_KIND" => word_field(&fields, "server")
            .zip(word_field(&fields, "kind"))
            .map(|(server, kind)| Body::Kind { server, kind }),
        "D2_HELD" => word_field(&fields, "rule").map(|rule| Body::Held { rule }),
        "L_STEP_ASIDE" => Some(Body::Aside {
            held: match field(&fields, "held") {
                Some(Val::Num(n)) => *n,
                _ => 0,
            },
        }),
        "E1_CLOSED" => match field(&fields, "code") {
            Some(Val::Num(code)) => u16::try_from(*code).ok().map(|code| Body::Closed { code }),
            _ => None,
        },
        "D4_DROP_ALONE" => match word_field(&fields, "outcome").as_deref() {
            Some("Removed") => Some(Body::DropAlone { removed: true }),
            Some("NotReached") => Some(Body::DropAlone { removed: false }),
            _ => None,
        },
        "D4_DROP_NO_ANSWER" => Some(Body::DropNoAnswer {
            live: field(&fields, "written").is_none(),
        }),
        _ => Some(Body::Plain),
    };
    Some(Parsed {
        row,
        level,
        body: body.unwrap_or(Body::Unreadable),
    })
}
