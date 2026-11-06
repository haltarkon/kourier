//! The line table and its parser (T5): the TWIN of TH6's contract (every const of the backend's
//! `conn/lines.rs` found here and every row of the table found there in its exact words, the count
//! line's fields in `Frames::log`'s order, the close codes of `close.rs`) and the parser's shapes,
//! counts and names only.

use std::collections::{BTreeMap, BTreeSet};

use super::{digest_shape, id_shape};
use crate::threema_run::judge::HOLD_RULES;
use crate::threema_run::lines::L_STEP_ASIDE;
use crate::threema_run::lines::{
    APP_THREEMA, Body, CLOSE_FILE, CONN_LINES, COUNT_FIELDS, Count, E6_LIST_NOT_USED, LINES, Level,
    NAMED_CLOSES, Parsed, parse,
};

fn source(path: &str) -> String {
    std::fs::read_to_string(crate::root().join(path))
        .unwrap_or_else(|e| panic!("{path} cannot be read: {e}"))
}

/// Every `const NAME: &str` of a Rust source with its literal (on its line or the next: a long one
/// wraps after the `=`).
fn str_consts(text: &str) -> BTreeMap<String, String> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = BTreeMap::new();
    for (at, line) in lines.iter().enumerate() {
        let Some(rest) = line.trim_start().strip_prefix("pub(crate) const ") else {
            continue;
        };
        let Some((name, tail)) = rest.split_once(": &str =") else {
            continue;
        };
        let literal_line = if tail.trim().is_empty() {
            lines.get(at + 1).copied().unwrap_or_default()
        } else {
            tail
        };
        let literal = literal_line
            .trim()
            .trim_end_matches(';')
            .trim_matches('"')
            .to_owned();
        out.insert(name.to_owned(), literal);
    }
    out
}

/// THE TWIN: every const of TH6's line file is a row of TR's table, every row of the table stands
/// there in its exact words, and E6 stands in the app as the literal of a WARN. A word TH6 changes,
/// a line TH6 adds or takes away: this fails the same day.
#[test]
fn the_twin_finds_every_line_of_the_backends_file_in_its_exact_words() {
    let consts = str_consts(&source(CONN_LINES));
    assert!(
        consts.len() >= 19,
        "the backend's file holds its lines: {consts:?}"
    );
    let ours: BTreeMap<&str, &str> = LINES
        .iter()
        .filter(|r| r.file == CONN_LINES)
        .map(|r| (r.name.expect("a const's row has its name"), r.text))
        .collect();
    let theirs: BTreeSet<&str> = consts.keys().map(String::as_str).collect();
    let known: BTreeSet<&str> = ours.keys().copied().collect();
    assert_eq!(
        theirs, known,
        "TH6's line file and TR's table name the same lines"
    );
    for (name, text) in &ours {
        assert_eq!(consts[*name], *text, "{name}: the words differ");
    }
    let app = source(APP_THREEMA);
    let e6 = app
        .lines()
        .find(|l| l.contains(&format!("\"{E6_LIST_NOT_USED}\"")))
        .expect("E6 stands in the app");
    assert!(e6.contains("tracing::warn!"), "E6 is a WARN: {e6}");
}

/// T-H1's step-aside line: the backend's const `L_STEP_ASIDE` (TH6 FIX-B moved the words from a literal
/// in `conn/mod.rs` into `conn/lines.rs`), a row of the same table as the others, printed by an INFO
/// call of the connection that names the const (no second copy of the words), read with its `held`
/// count, and never confused with D2's words.
#[test]
fn the_step_aside_line_is_the_backends_const_and_is_read_by_its_row() {
    let row = LINES
        .iter()
        .find(|r| r.text == L_STEP_ASIDE)
        .expect("the table reads the step-aside line");
    assert_eq!(
        (row.file, row.name, row.level),
        (CONN_LINES, Some("L_STEP_ASIDE"), Level::Info)
    );
    assert_eq!(row.fields, ["held"]);
    let conn = source("crates/kourier-threema/src/conn/mod.rs");
    assert!(
        !conn.contains(&format!("\"{L_STEP_ASIDE}\"")),
        "the words stand in `conn/lines.rs` alone, the call names the const"
    );
    let at = conn
        .find("lines::L_STEP_ASIDE")
        .expect("the connection prints the const");
    let before = &conn[..at];
    assert!(
        before[before.len().saturating_sub(120)..].contains("tracing::info!("),
        "it is an INFO call"
    );
    let line = at_info(&format!("{L_STEP_ASIDE} held=2"));
    let parsed = parse(&line).expect("the step-aside line");
    assert_eq!(parsed.row, "L_STEP_ASIDE");
    assert_eq!(parsed.body, Body::Aside { held: 2 });
    assert_eq!(parsed.level, Some(Level::Info));
    // D2's words and the step-aside's are two lines.
    let held = parse(&at_info(
        "a chat server message is held for the phone rule=csp-unknown-type",
    ))
    .expect("D2");
    assert_eq!(held.row, "D2_HELD");
    // No field, a quoted or a wordy one: a count of 0, never a guess.
    assert_eq!(
        parse(&at_info(L_STEP_ASIDE)).expect("a line").body,
        Body::Aside { held: 0 }
    );
}

/// The hold words of `th6-lines.md` ("After L6", "After the fix round (TH6 FIX-A)": twelve): each stands
/// as a quoted literal in the backend's sources (the tables it names them in), and the two words L5 and
/// L6 took away stand nowhere.
#[test]
fn the_hold_words_are_the_backends_and_the_two_gone_ones_stand_nowhere() {
    fn walk(dir: &std::path::Path, out: &mut String) {
        for entry in std::fs::read_dir(dir).expect("a directory").flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push_str(&std::fs::read_to_string(&path).unwrap_or_default());
            }
        }
    }
    let mut src = String::new();
    walk(&crate::root().join("crates/kourier-threema/src"), &mut src);
    for rule in HOLD_RULES {
        assert!(
            src.contains(&format!("\"{rule}\"")),
            "the hold word {rule} is the backend's"
        );
    }
    for gone in ["csp-sender-lookup", "inbound-jobs-pending"] {
        assert!(
            !src.contains(&format!("\"{gone}\"")),
            "{gone} was taken away and must not come back unread"
        );
    }
}

fn at_info(text: &str) -> String {
    at("INFO", "kourier_threema::conn", text)
}

/// The count line's fields are `Frames::log`'s, in its order.
#[test]
fn the_count_lines_fields_are_frames_logs_in_its_order() {
    let text = source(CONN_LINES);
    let start = text
        .find("pub(crate) fn log(&self, when: When)")
        .expect("Frames::log");
    let body = &text[start..];
    let call = &body[body.find("tracing::info!(").expect("its call")..];
    let call = &call[..call.find("\"{L_COUNT}\"").expect("the count line's words")];
    let names: Vec<&str> = call
        .lines()
        .skip(1)
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| {
            l.trim_end_matches(',')
                .split([' ', '='])
                .next()
                .expect("a field")
        })
        .collect();
    assert_eq!(names, COUNT_FIELDS, "the fields in the call's order");
}

/// The close codes whose words the spec gives are `close::sentence`'s arms.
#[test]
fn the_close_codes_the_spec_names_are_close_rs_arms() {
    let text = source(CLOSE_FILE);
    let start = text.find("pub fn sentence(code: u16)").expect("sentence");
    let body = &text[start..];
    let body = &body[..body.find("_ => return None").expect("its last arm")];
    let codes: BTreeSet<u16> = body
        .lines()
        .filter_map(|l| l.trim().split_once(" => \"")?.0.parse().ok())
        .collect();
    assert_eq!(codes, NAMED_CLOSES.into_iter().collect::<BTreeSet<u16>>());
}

/// A line as the app prints it (tracing's plain format), at `level` from `target`.
pub(super) fn at(level: &str, target: &str, text: &str) -> String {
    format!("2026-11-04T10:00:00.000000Z {level:>5} {target}: {text}")
}

/// A whole count line: every counter 0 but those `set`, `held_by` as given (`held` its sum).
pub(super) fn count_line(when: &str, set: &[(&str, u64)], held_by: &[(&str, u64)]) -> String {
    let mut fields = vec![format!("when={when}")];
    for name in &COUNT_FIELDS[1..27] {
        let n = set.iter().find(|(f, _)| f == name).map_or(0, |(_, n)| *n);
        fields.push(format!("{name}={n}"));
    }
    let held: u64 = held_by.iter().map(|(_, n)| n).sum();
    fields.push(format!("held={held}"));
    let by = if held_by.is_empty() {
        "none".to_owned()
    } else {
        held_by
            .iter()
            .map(|(r, n)| format!("{r}:{n}"))
            .collect::<Vec<_>>()
            .join(",")
    };
    fields.push(format!("held_by={by}"));
    at(
        "INFO",
        "kourier_threema::conn::lines",
        &format!("socket frame counts {}", fields.join(" ")),
    )
}

fn body(line: &str) -> Body {
    parse(line).expect("a line of the table").body
}

/// The parser's shapes: a whole count line, each line with fields, the belts, and what is no line of
/// the table; a line whose fields are not the contract's is unreadable, never a guess.
#[test]
fn the_parser_reads_counts_and_names_in_the_contracts_shapes() {
    // The count line, whole.
    let line = count_line(
        "end",
        &[("in_csp_message", 2), ("out_box_job", 1)],
        &[("csp-unknown-type", 1), ("group-sync-needed", 2)],
    );
    let Body::Count(c) = body(&line) else {
        panic!("a count: {line}")
    };
    assert!(c.end);
    assert_eq!(
        (c.get("in_csp_message"), c.get("out_box_job"), c.get("held")),
        (2, 1, 3)
    );
    assert_eq!(
        c.held_by,
        [
            ("csp-unknown-type".to_owned(), 1),
            ("group-sync-needed".to_owned(), 2)
        ]
    );
    let Body::Count(Count { end, .. }) = body(&count_line("running", &[], &[])) else {
        panic!("a running count")
    };
    assert!(!end);
    assert_eq!(
        parse(&count_line("end", &[], &[])).expect("a line").level,
        Some(Level::Info)
    );
    // What is not the contract's count line: a field missing, two swapped, a word for a number,
    // `held` that is not the sum, an unknown `when`.
    let whole = count_line("end", &[], &[]);
    let unreadable = [
        whole.replace(" in_dry=0", ""),
        whole.replace("in_dry=0 in_promoted=0", "in_promoted=0 in_dry=0"),
        whole.replace("in_dry=0", "in_dry=some"),
        whole.replace("held=0", "held=1"),
        whole.replace("when=end", "when=later"),
        whole.replace("held_by=none", "held_by=a:b"),
    ];
    for line in unreadable {
        assert_eq!(body(&line), Body::Unreadable, "{line}");
    }
    // The other lines whose fields the judge reads.
    let rows = [
        (
            at(
                "INFO",
                "kourier_threema::conn",
                "the mediator closed the connection code=4113 meaning=\"Dropped by other device\"",
            ),
            Body::Closed { code: 4113 },
        ),
        (
            at(
                "INFO",
                "kourier_threema::conn",
                "the mediator closed the connection meaning=\"Normal closure\"",
            ),
            Body::Unreadable,
        ),
        (
            at(
                "INFO",
                "kourier_threema::conn::lines",
                "a server payload Kourier does not handle server=chat kind=ServerAlert",
            ),
            Body::Kind {
                server: "chat".to_owned(),
                kind: "ServerAlert".to_owned(),
            },
        ),
        (
            at(
                "INFO",
                "kourier_threema::conn::lines",
                "a chat server message is held for the phone rule=group-sync-needed",
            ),
            Body::Held {
                rule: "group-sync-needed".to_owned(),
            },
        ),
        (
            at(
                "INFO",
                "kourier_threema::conn::drop_own",
                "the own device's drop on a socket of its own outcome=Removed",
            ),
            Body::DropAlone { removed: true },
        ),
        (
            at(
                "INFO",
                "kourier_threema::conn::drop_own",
                "the own device's drop on a socket of its own outcome=NotReached",
            ),
            Body::DropAlone { removed: false },
        ),
        // RT1 attempt 1: E4 with the error's fixed words is a line of the table the tool reads (no field it keeps).
        (
            at(
                "WARN",
                "kourier_threema::conn",
                "the server's message was refused what=\"d2m\" unsupported=false kind=unexpected-payload-type payload_type=0",
            ),
            Body::Plain,
        ),
        (
            at(
                "WARN",
                "kourier_threema::conn::drop_own",
                "the server's message was refused what=\"d2m\" code=4010 kind=decoding-failed name=ServerInfo",
            ),
            Body::Plain,
        ),
        (
            at(
                "INFO",
                "kourier_threema::conn",
                "the mediator did not answer the drop in time",
            ),
            Body::DropNoAnswer { live: true },
        ),
        (
            at(
                "INFO",
                "kourier_threema::conn::drop_own",
                "the mediator did not answer the drop in time written=true",
            ),
            Body::DropNoAnswer { live: false },
        ),
        (
            at(
                "WARN",
                "kourier_threema::conn",
                "the server's message was refused what=\"csp chunks\" unsupported=false",
            ),
            Body::Plain,
        ),
        (
            at(
                "WARN",
                "kourier_threema::conn",
                "the clock is off by more than 20 minutes server=\"chat server\" minutes=25",
            ),
            Body::Plain,
        ),
    ];
    for (line, want) in rows {
        assert_eq!(body(&line), want, "{line}");
    }
    // E6 carries a word of prose: its row is known, its fields are not read.
    let e6 = at(
        "WARN",
        "kourier_app::threema",
        "the list of Threema test IDs is not used problem=the file is not 0600",
    );
    assert_eq!(parse(&e6).expect("E6").row, "E6_LIST_NOT_USED");
    // The belts: an ERROR line names its crate (spans may stand before the target); a panic.
    let belts = [
        (
            at("ERROR", "kourier_core::ingest", "a write failed: busy"),
            Some("kourier_core"),
        ),
        (
            "2026-11-04T10:00:00Z ERROR actor{account=1 net=x}: kourier_threema::conn: broke"
                .to_owned(),
            Some("kourier_threema"),
        ),
        (
            at("ERROR", "kourier_qt", "a QML warning"),
            Some("kourier_qt"),
        ),
    ];
    for (line, want) in belts {
        let p = parse(&line).expect("a belt");
        assert_eq!(p.row, "ERROR_LINE", "{line}");
        assert_eq!(
            p.body,
            Body::Belt {
                error_of: want.map(str::to_owned)
            },
            "{line}"
        );
    }
    assert_eq!(
        parse("thread 'tokio-runtime-worker' panicked at src/x.rs:1:1").map(|p| p.row),
        Some("PANIC")
    );
    assert_eq!(
        parse(&at("INFO", "kourier_core", "a plain line count=3")),
        None
    );
}

/// Counts and names only: a value that is no number and no word, a quoted value, an ID's shape or a
/// digest's in a field never reach what the parser keeps.
#[test]
fn the_parser_keeps_no_value_but_counts_and_words() {
    let (id, digest) = (id_shape(), digest_shape());
    let lines = [
        at(
            "INFO",
            "kourier_threema::conn",
            &format!("the mediator closed the connection code=4000 meaning=\"{id} {digest}\""),
        ),
        at(
            "INFO",
            "kourier_threema::conn::lines",
            &format!("a chat server message is held for the phone rule=x/{digest}"),
        ),
        at(
            "WARN",
            "kourier_threema::conn",
            &format!("the server's certificate is not one Kourier trusts kind=\"{id}\""),
        ),
    ];
    for line in lines {
        let kept: Parsed = parse(&line).expect("a line of the table");
        let shown = format!("{kept:?}");
        assert!(
            !shown.contains(&id) && !shown.contains(&digest),
            "a value was kept: {shown}"
        );
    }
}
