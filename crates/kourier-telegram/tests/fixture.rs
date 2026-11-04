//! The scrubbed login fixture (`testdata/login-flow.jsonl`, `testdata/login-rules.jsonl`, TG-2 C0):
//! TDLib 1.8.68's output against the spike's fake data centre with every name, number and text
//! replaced by synthetic values. These tests re-check in Rust what the scrub asserted, so a later
//! fixture that brings a real-looking value back fails here (critic H5). A failing assertion names
//! the CHECK, never the value it found.
//!
//! What this file checks, and what only the scrub script checks (`scripts/tg-fixture-scrub.py`,
//! which needs the spike's ORIGINAL flow and so cannot run in a test): here the result is checked
//! (the synthetic values, the host names, the reviewed list of string-valued key paths in
//! `testdata/string-paths.txt` that the script reads too, the dates inside the synthetic window,
//! the UTC offset 0); in the script that no original value (name, number, title, username,
//! signature, text, date) survives, with the counters it prints.
//!
//! TD-4a C7 (brief §3.8): EVERY `testdata/*.jsonl` is read at RUN time (`fixture_files`), so a new
//! recording (`rec-<name>.jsonl`, `cargo xtask tdlib fakedc record`) is checked without editing
//! this file; the ONE exception is [`RULES_FILE`] (critic L-k): it holds `ScriptedTd`'s rules, not
//! TDLib's output, and is checked through its rules' `reply` objects below.

use kourier_tdlib::scripted::Script;
use serde_json::Value;

const FLOW: &str = include_str!("../testdata/login-flow.jsonl");
const RULES: &str = include_str!("../testdata/login-rules.jsonl");
/// The reviewed list of string-valued key paths, one per line (`#` lines are comments); the scrub
/// script reads the same file. A string at a path that is not on it fails the scrub check.
const STRING_PATHS: &str = include_str!("../testdata/string-paths.txt");

/// The fixed synthetic "now" of the scrub (2023-11-14T22:13:20Z) and the window around it that a
/// date of the fixtures must lie in (the script keeps the seconds between the original dates).
const SYNTH_EPOCH: i64 = 1_700_000_000;
const SYNTH_WINDOW: i64 = 3_600;
/// A recording's dates start at the epoch and keep their distances (the scrub's `REC_WINDOW`).
const REC_WINDOW: i64 = 6 * 3_600;
/// The one `.jsonl` of `testdata/` that is not TDLib output (critic L-k): `ScriptedTd`'s rules.
const RULES_FILE: &str = "login-rules.jsonl";
/// The marker and step lines of a recording (the test driver's, P-10): known types.
const RECORDING_TYPES: [&str; 2] = ["kourierRequest", "kourierStep"];
/// The options that are dates (every other option's number is a limit or an id).
const DATE_OPTIONS: [&str; 2] = ["unix_time", "authorization_date"];

/// The synthetic own number (the run tool's tests type it), as TDLib writes it in `code_info`
/// (`+`) and in `user` (digits only).
const OWN_NUMBER: &str = "+999068204719";
const OWN_DIGITS: &str = "999068204719";
/// The synthetic peer's number.
const PEER_DIGITS: &str = "999075390162";
/// The synthetic login code of the rules file.
const CODE: &str = "12345";

const NAMES: [&str; 4] = ["Ada", "Test", "Bo", "Peer"];
const TITLES: [&str; 2] = ["Ada Test", "Bo Peer"];
/// The further synthetic people of the recordings (the scrub's `POOL` after Bo Peer).
const REC_PEOPLE: [(&str, &str, &str); 4] = [
    ("Cy", "Sample", "999081736254"),
    ("Di", "Sample", "999092647315"),
    ("Ed", "Sample", "999013572468"),
    ("Fay", "Sample", "999024681357"),
];

/// What `testdata/` may hold (S-L5): the README, the reviewed string paths, the login fixtures,
/// the recordings and (TG-3 C1, L-14) the synthetic receive lines `receive-*.jsonl` (checked like
/// the login flow: synthetic values, listed paths, the 1 h window) and the content list
/// `message-contents.txt` (C4a). Anything else (a `<name>.world.json`, a raw tape, a wire log) is CC0 by the
/// `testdata/*` row and checked by nothing, so it fails here.
fn testdata_allowed(name: &str) -> bool {
    name == "README"
        || name == "string-paths.txt"
        || (name.starts_with("login-") && name.ends_with(".jsonl"))
        || (name.starts_with("rec-") && name.ends_with(".jsonl"))
        || (name.starts_with("receive-") && name.ends_with(".jsonl"))
        || name == "message-contents.txt"
}

#[test]
fn testdata_holds_only_what_is_checked() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata");
    let strangers: Vec<String> = std::fs::read_dir(&dir)
        .expect("testdata/")
        .map(|e| {
            e.expect("a testdata entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .filter(|n| !testdata_allowed(n))
        .collect();
    assert_eq!(
        strangers,
        Vec::<String>::new(),
        "testdata/ holds a file nothing checks"
    );
    assert!(!testdata_allowed("rec-x.world.json"));
    assert!(!testdata_allowed("rec-x.raw.jsonl.bak"));
    assert!(!testdata_allowed("wire.jsonl"));
    assert!(testdata_allowed("rec-fresh-start.jsonl"));
    assert!(testdata_allowed("receive-kinds.jsonl"));
    assert!(!testdata_allowed("receive-kinds.json"));
}

/// Every `testdata/*.jsonl` but [`RULES_FILE`], read now: (file name, text), sorted by name.
fn fixture_files() -> Vec<(String, String)> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata");
    let mut files: Vec<(String, String)> = std::fs::read_dir(&dir)
        .expect("testdata/")
        .map(|e| e.expect("a testdata entry").path())
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .filter_map(|p| {
            let name = p.file_name()?.to_str()?.to_owned();
            (name != RULES_FILE).then(|| (name, std::fs::read_to_string(&p).expect("read")))
        })
        .collect();
    files.sort();
    files
}

/// The recordings among the fixture files.
fn recordings() -> Vec<(String, String)> {
    fixture_files()
        .into_iter()
        .filter(|(n, _)| n.starts_with("rec-"))
        .collect()
}

fn is_synthetic_number(v: &str) -> bool {
    [OWN_NUMBER, OWN_DIGITS, PEER_DIGITS].contains(&v) || REC_PEOPLE.iter().any(|p| p.2 == v)
}

fn is_synthetic_name(v: &str) -> bool {
    NAMES.contains(&v) || REC_PEOPLE.iter().any(|p| p.0 == v || p.1 == v)
}

/// A chat title: a person's full name, `Group <n>` or `Channel <n>`.
fn is_synthetic_title(v: &str) -> bool {
    let numbered = |prefix: &str| {
        v.strip_prefix(prefix)
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
    };
    TITLES.contains(&v)
        || REC_PEOPLE.iter().any(|p| v == format!("{} {}", p.0, p.1))
        || numbered("Group ")
        || numbered("Channel ")
}

/// A username: `synthetic_<n>` (TF17: the rule of the recordings, now for every file).
fn is_synthetic_username(v: &str) -> bool {
    v.strip_prefix("synthetic_")
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// A link: its host is `example.org` or `example.invalid` (TF17), whatever the scheme.
fn is_synthetic_url(v: &str) -> bool {
    v.split_once("://").is_some_and(|(_, rest)| {
        let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
        host == "example.org" || host == "example.invalid"
    })
}

/// A file name, an address or a remote id of a synthetic file: it starts with `synthetic` (TF17).
fn is_synthetic_handle(v: &str) -> bool {
    v.is_empty() || v.starts_with("synthetic")
}

/// Every `remoteFile` object's ids: TDLib's handles that name a file for an account.
fn remote_ids<'a>(v: &'a Value, out: &mut Vec<&'a str>) {
    match v {
        Value::Array(items) => items.iter().for_each(|i| remote_ids(i, out)),
        Value::Object(map) => {
            if map.get("@type").and_then(Value::as_str) == Some("remoteFile") {
                out.extend(
                    ["id", "unique_id"]
                        .iter()
                        .filter_map(|k| map.get(*k)?.as_str()),
                );
            }
            map.values().for_each(|i| remote_ids(i, out));
        }
        _ => {}
    }
}

/// Telegram's host names, assembled at run time (the source check allows them in one file only).
fn hosts() -> Vec<String> {
    vec![
        ["tele", "gram.org"].concat(),
        ["t", ".me"].concat(),
        ["tele", "gram.me"].concat(),
        ["telegra", ".ph"].concat(),
    ]
}

fn json_lines(text: &str) -> Vec<Value> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .enumerate()
        .map(|(n, l)| {
            serde_json::from_str(l).unwrap_or_else(|_| panic!("line {} is not JSON", n + 1))
        })
        .collect()
}

/// Every (key, string value) pair of a JSON value, depth first.
fn strings<'a>(v: &'a Value, key: &'a str, out: &mut Vec<(&'a str, &'a str)>) {
    match v {
        Value::String(s) => out.push((key, s.as_str())),
        Value::Array(items) => items.iter().for_each(|i| strings(i, key, out)),
        Value::Object(map) => map.iter().for_each(|(k, i)| strings(i, k.as_str(), out)),
        _ => {}
    }
}

/// Every `formattedText` object's `text`.
fn texts<'a>(v: &'a Value, out: &mut Vec<&'a str>) {
    match v {
        Value::Array(items) => items.iter().for_each(|i| texts(i, out)),
        Value::Object(map) => {
            if map.get("@type").and_then(Value::as_str) == Some("formattedText")
                && let Some(t) = map.get("text").and_then(Value::as_str)
            {
                out.push(t);
            }
            map.values().for_each(|i| texts(i, out));
        }
        _ => {}
    }
}

/// The reviewed string-valued key paths.
fn reviewed_paths() -> Vec<&'static str> {
    STRING_PATHS
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect()
}

/// Every string-valued key path of a JSON value: the keys joined with `/`, `[]` for a list item
/// (`levels/[]/@type`).
fn string_paths(v: &Value, path: &str, out: &mut Vec<String>) {
    let child = |key: &str| {
        if path.is_empty() {
            key.to_owned()
        } else {
            format!("{path}/{key}")
        }
    };
    match v {
        Value::String(_) => out.push(path.to_owned()),
        Value::Array(items) => items
            .iter()
            .for_each(|i| string_paths(i, &child("[]"), out)),
        Value::Object(map) => map
            .iter()
            .for_each(|(k, i)| string_paths(i, &child(k), out)),
        _ => {}
    }
}

/// Every integer (a number, or a string of digits as TDLib writes an int64) that could be a Unix
/// date (1e9 to 4e9). An option that is no date option is skipped (a limit or an id such as
/// `anti_spam_bot_user_id` can lie in that range).
fn date_like(v: &Value, out: &mut Vec<i64>) {
    match v {
        Value::Number(n) => out.extend(
            n.as_i64()
                .filter(|n| (1_000_000_000..=4_000_000_000).contains(n)),
        ),
        Value::String(s) => out.extend(
            s.parse::<i64>()
                .ok()
                .filter(|n| (1_000_000_000..=4_000_000_000).contains(n)),
        ),
        Value::Array(items) => items.iter().for_each(|i| date_like(i, out)),
        Value::Object(map) => {
            let option = map.get("@type").and_then(Value::as_str) == Some("updateOption");
            let name = map.get("name").and_then(Value::as_str).unwrap_or_default();
            if !option || DATE_OPTIONS.contains(&name) {
                map.values().for_each(|i| date_like(i, out));
            }
        }
        _ => {}
    }
}

/// What a fixture line holds: the line itself, or, in the rules file, every object of its `reply`
/// (the rule's own `on` and `when` hold the request name and the synthetic number and code).
fn objects(line: &Value) -> Vec<&Value> {
    match line.get("reply").and_then(Value::as_array) {
        Some(reply) if line.get("on").is_some() => reply.iter().collect(),
        _ => vec![line],
    }
}

/// Every `proxy` object of a JSON value (the answer to the helper's `addProxy`).
fn proxy_objects<'a>(v: &'a Value, out: &mut Vec<&'a Value>) {
    match v {
        Value::Array(items) => items.iter().for_each(|i| proxy_objects(i, out)),
        Value::Object(map) => {
            if map.get("@type").and_then(Value::as_str) == Some("proxy") {
                out.push(v);
            }
            map.values().for_each(|i| proxy_objects(i, out));
        }
        _ => {}
    }
}

/// The checks of one file; returns the names of the checks that failed (never a value).
fn problems(text: &str) -> Vec<&'static str> {
    problems_in(text, -SYNTH_WINDOW, SYNTH_WINDOW)
}

/// The checks of one recording: [`problems`] with the recording's window, plus the recording's
/// own rules (usernames `synthetic_<n>`, `@extra` renumbered 1..n, markers never of a secret).
fn recording_problems(text: &str) -> Vec<&'static str> {
    let mut bad = problems_in(text, 0, REC_WINDOW);
    let lines = json_lines(text);
    let mut extras: Vec<i64> = lines.iter().filter_map(|l| l["@extra"].as_i64()).collect();
    if lines
        .iter()
        .any(|l| l.get("@extra").is_some_and(|e| !e.is_i64()))
    {
        bad.push("an @extra that is not a number");
    }
    extras.sort_unstable();
    extras.dedup();
    if extras.iter().enumerate().any(|(i, e)| *e != i as i64 + 1) {
        bad.push("@extra not renumbered 1..n");
    }
    for l in &lines {
        if l["@type"] == "kourierRequest" {
            let r = l["request"].as_str().unwrap_or_default();
            if r == "setTdlibParameters"
                || r.starts_with("setAuthentication")
                || r.starts_with("checkAuthentication")
                || r.starts_with("requestQrCode")
                || r.contains("Password")
                || r.contains("Token")
            {
                bad.push("a marker of a secret-bearing request");
            }
        }
        let mut pairs = Vec::new();
        strings(l, "", &mut pairs);
        // S-L1: the gate's per-run SOCKS5 pair is blank and the free-text fields hold nothing
        // (the scrub replaces exact originals of the fake world only, and the world serves none).
        let mut proxies = Vec::new();
        proxy_objects(l, &mut proxies);
        if proxies.iter().any(|p| {
            p["server"] != "127.0.0.1"
                || p["port"] != 1080
                || ["username", "password", "secret"]
                    .iter()
                    .any(|k| p["type"].get(k).is_some_and(|v| v != ""))
        }) {
            bad.push("a proxy that is not the blank 127.0.0.1:1080 entry");
        }
        if pairs.iter().any(|(k, v)| {
            matches!(*k, "description" | "tag" | "secondary_username") && !v.is_empty()
        }) {
            bad.push("a description, tag or secondary username that is not blank");
        }
    }
    bad.sort_unstable();
    bad.dedup();
    bad
}

fn problems_in(text: &str, from: i64, to: i64) -> Vec<&'static str> {
    let mut bad = Vec::new();
    if hosts().iter().any(|h| text.contains(h.as_str())) {
        bad.push("a Telegram host name");
    }
    let reviewed = reviewed_paths();
    for line in json_lines(text) {
        for object in objects(&line) {
            let mut paths = Vec::new();
            string_paths(object, "", &mut paths);
            if paths.iter().any(|p| !reviewed.contains(&p.as_str())) {
                bad.push("a string path that is not on the reviewed list");
            }
            let mut dates = Vec::new();
            date_like(object, &mut dates);
            if dates
                .iter()
                .any(|d| !(SYNTH_EPOCH + from..=SYNTH_EPOCH + to).contains(d))
            {
                bad.push("a date outside the synthetic window");
            }
            if object.get("@type").and_then(Value::as_str) == Some("updateOption")
                && object.get("name").and_then(Value::as_str) == Some("utc_time_offset")
                && object["value"]["value"].as_str() != Some("0")
            {
                bad.push("a UTC offset that is not 0");
            }
        }
        let mut pairs = Vec::new();
        strings(&line, "", &mut pairs);
        for (key, value) in pairs {
            match key {
                "phone_number" if !value.is_empty() && !is_synthetic_number(value) => {
                    bad.push("a phone number that is not synthetic")
                }
                "first_name" | "last_name" if !is_synthetic_name(value) => {
                    bad.push("a name that is not synthetic")
                }
                "title" if !is_synthetic_title(value) => {
                    bad.push("a chat title that is not synthetic")
                }
                "author_signature" | "sender_tag" | "client_data" if !value.is_empty() => {
                    bad.push("a signature, sender tag or client data that is not blank")
                }
                // TF17: every file gets the recordings' rules for what names a person or a file.
                "active_usernames" | "editable_username" | "username"
                    if !value.is_empty() && !is_synthetic_username(value) =>
                {
                    bad.push("a username that is not synthetic")
                }
                "url" if !is_synthetic_url(value) => {
                    bad.push("a link that is not on example.org or example.invalid")
                }
                "file_name" | "address" if !is_synthetic_handle(value) => {
                    bad.push("a file name or address that is not synthetic")
                }
                _ => {}
            }
        }
        let mut remote = Vec::new();
        remote_ids(&line, &mut remote);
        if remote.iter().any(|id| !is_synthetic_handle(id)) {
            bad.push("a remote file id that is not synthetic");
        }
        let mut found = Vec::new();
        texts(&line, &mut found);
        if found.iter().any(|t| !t.starts_with("synthetic text ")) {
            bad.push("a message text that is not synthetic");
        }
    }
    bad.sort_unstable();
    bad.dedup();
    bad
}

#[test]
fn the_fixture_is_scrubbed() {
    assert_eq!(problems(FLOW), Vec::<&str>::new(), "login-flow.jsonl");
    assert_eq!(problems(RULES), Vec::<&str>::new(), "login-rules.jsonl");
    // The synthetic values are really there (a scrub that dropped the fields would pass above).
    assert!(FLOW.contains(OWN_NUMBER), "the own number is in the flow");
    assert!(
        FLOW.contains(PEER_DIGITS),
        "the peer's number is in the flow"
    );
    // The code is typed by a test, never part of what TDLib said.
    assert!(
        !FLOW.contains(CODE),
        "the synthetic code is not in the flow"
    );
    // Requests' names were the spike's: none survives.
    assert!(!FLOW.contains("\"@extra\""), "no @extra in the flow");
    assert_eq!(json_lines(FLOW).len(), 176, "the whole spike flow");
    // Read at run time: the flow is among the files, and every recording passes its checks.
    let files = fixture_files();
    assert!(
        files
            .iter()
            .any(|(n, t)| n == "login-flow.jsonl" && t == FLOW),
        "login-flow.jsonl is read at run time"
    );
    for (name, text) in &files {
        if name.starts_with("rec-") {
            assert_eq!(recording_problems(text), Vec::<&str>::new(), "{name}");
            assert!(!text.contains(CODE), "{name}: the synthetic code");
        } else {
            assert_eq!(problems(text), Vec::<&str>::new(), "{name}");
        }
    }
}

/// TF17 (review pF8): EVERY file gets the recordings' username rule and the link, file name,
/// address and remote id rules: a hand-written line with a real-looking value fails, its synthetic
/// twin passes. (Mutant: `synthetic_7` -> a real-looking username in `receive-kinds.jsonl`.)
#[test]
fn every_file_gets_the_username_link_and_handle_rules() {
    let user = |name: &str| {
        format!(
            r#"{{"@type":"updateUser","user":{{"@type":"user","usernames":{{"@type":"usernames","active_usernames":["{name}"],"editable_username":"{name}"}}}}}}"#
        )
    };
    assert_eq!(problems(&user("synthetic_7")), Vec::<&str>::new());
    assert_eq!(
        problems(&user("ada_lovelace")),
        vec!["a username that is not synthetic"]
    );
    let link = |url: &str| {
        format!(
            r#"{{"@type":"updateNewMessage","message":{{"@type":"message","content":{{"@type":"messageText","text":{{"@type":"formattedText","text":"synthetic text 1","entities":[{{"@type":"textEntity","offset":0,"length":1,"type":{{"@type":"textEntityTypeTextUrl","url":"{url}"}}}}]}}}}}}}}"#
        )
    };
    for fine in [
        "https://example.org/hidden",
        "ftp://example.org/synthetic",
        "https://example.invalid/x?y=1",
    ] {
        assert_eq!(problems(&link(fine)), Vec::<&str>::new(), "{fine}");
    }
    // (A Telegram link is assembled at run time: the source check allows the host name in one file.)
    let telegram_link = ["https://t", ".me/real"].concat();
    for real in [
        "https://real-site.net/page",
        "https://example.org.evil.net/x",
        telegram_link.as_str(),
    ] {
        assert!(
            problems(&link(real)).contains(&"a link that is not on example.org or example.invalid"),
            "{real} passes"
        );
    }
    let document = |name: &str, remote: &str| {
        format!(
            r#"{{"@type":"updateNewMessage","message":{{"@type":"message","content":{{"@type":"messageDocument","document":{{"@type":"document","file_name":"{name}","document":{{"@type":"file","remote":{{"@type":"remoteFile","id":"{remote}","unique_id":"synthetic-unique-1"}}}}}}}}}}}}"#
        )
    };
    assert_eq!(
        problems(&document("synthetic-4.pdf", "synthetic-remote-5")),
        Vec::<&str>::new()
    );
    assert_eq!(
        problems(&document("report.pdf", "synthetic-remote-5")),
        vec!["a file name or address that is not synthetic"]
    );
    assert_eq!(
        problems(&document("synthetic-4.pdf", "AgADreal")),
        vec!["a remote file id that is not synthetic"]
    );
}

/// S-L1's positive control: the guard sees a non-empty proxy credential, another proxy address, and
/// a non-blank description, member tag or secondary username in a COPY of a real recording.
#[test]
fn the_recording_guards_see_a_proxy_pair_and_free_text() {
    let (name, text) = recordings()
        .into_iter()
        .find(|(_, t)| t.contains("\"proxyTypeSocks5\""))
        .expect("a recording that holds the gate's proxy");
    assert_eq!(recording_problems(&text), Vec::<&str>::new(), "{name}");
    let marks = [
        (r#""password":"""#, r#""password":"x""#),
        (
            r#""username":"","password""#,
            r#""username":"u","password""#,
        ),
        (r#""port":1080"#, r#""port":1081"#),
        (r#""server":"127.0.0.1""#, r#""server":"10.0.0.1""#),
    ];
    for (from, to) in marks {
        assert!(text.contains(from), "{name}: no {from}");
        let bad = text.replacen(from, to, 1);
        assert!(
            recording_problems(&bad)
                .contains(&"a proxy that is not the blank 127.0.0.1:1080 entry"),
            "{name}: {to} passes"
        );
    }
    for key in ["description", "tag", "secondary_username"] {
        let line = format!(r#"{{"@type":"updateX","{key}":"free text"}}"#);
        assert!(
            recording_problems(&format!("{text}{line}\n"))
                .contains(&"a description, tag or secondary username that is not blank"),
            "{key} passes"
        );
    }
}

/// Every recording holds TDLib's own lines, and an answer whose request has a marker comes after
/// that marker (`@extra` pairs a request with its answer, P-10); at least one recording has markers.
#[test]
fn the_recordings_pair_answers_with_their_markers() {
    for (name, text) in recordings() {
        let lines = json_lines(&text);
        let markers: Vec<i64> = lines
            .iter()
            .filter(|l| l["@type"] == "kourierRequest")
            .filter_map(|l| l["@extra"].as_i64())
            .collect();
        assert!(
            lines
                .iter()
                .any(|l| !RECORDING_TYPES.contains(&l["@type"].as_str().unwrap_or_default())),
            "{name}: no TDLib line"
        );
        // An answer's @extra has its marker before it (an unmarked request's answer is the one
        // exception: the login inputs and the parameters never get a marker).
        for (i, l) in lines.iter().enumerate() {
            if l["@type"] == "kourierRequest" {
                continue;
            }
            if let Some(e) = l["@extra"].as_i64()
                && markers.contains(&e)
            {
                let at = lines
                    .iter()
                    .position(|m| m["@type"] == "kourierRequest" && m["@extra"] == e)
                    .expect("the marker");
                assert!(at < i, "{name}: an answer before its marker");
            }
        }
    }
}

/// The reviewed list is exactly the string paths of the two files: no stale path stays on it (a
/// list that only grows would let a later fixture bring back a kind of text nobody looked at).
#[test]
fn the_reviewed_list_is_the_paths_the_fixtures_use() {
    let mut used = Vec::new();
    let files = fixture_files();
    for text in files.iter().map(|(_, t)| t.as_str()).chain([RULES]) {
        for line in json_lines(text) {
            for object in objects(&line) {
                string_paths(object, "", &mut used);
            }
        }
    }
    used.sort_unstable();
    used.dedup();
    let mut listed: Vec<String> = reviewed_paths().into_iter().map(str::to_owned).collect();
    listed.sort_unstable();
    listed.dedup();
    assert_eq!(
        listed.len(),
        reviewed_paths().len(),
        "a path is listed twice"
    );
    let unused = listed.iter().filter(|p| !used.contains(p)).count();
    let unlisted = used.iter().filter(|p| !listed.contains(p)).count();
    assert_eq!(
        (unused, unlisted),
        (0, 0),
        "the list and the files disagree"
    );
}

/// The checks catch what they must (a mutant written as a test): a real-looking chat title, a
/// host name and an unknown number each fail.
#[test]
fn the_scrub_checks_catch_a_real_looking_value() {
    let title = r#"{"@type":"updateNewChat","chat":{"@type":"chat","title":"Real Name"}}"#;
    assert_eq!(problems(title), vec!["a chat title that is not synthetic"]);
    let host = format!(
        r#"{{"@type":"updateOption","name":"t_me_url","value":{{"@type":"optionValueString","value":"https://{}/"}}}}"#,
        hosts()[1]
    );
    assert_eq!(problems(&host), vec!["a Telegram host name"]);
    let number = r#"{"@type":"user","phone_number":"999000000001"}"#;
    assert_eq!(
        problems(number),
        vec!["a phone number that is not synthetic"]
    );
}

/// J12 (the privacy review's MEDIUM): a string at a path nobody reviewed fails (a username, a bio,
/// a hint, a link: the kinds of text the scrub does not know), a date of the developer's machine
/// and its UTC offset fail, and an option that is only a number in the date range does not.
#[test]
fn the_scrub_checks_catch_an_unknown_string_path_a_real_date_and_an_offset() {
    let bio =
        r#"{"@type":"userFullInfo","bio":{"@type":"formattedText","text":"synthetic text 1"}}"#;
    assert_eq!(
        problems(bio),
        vec!["a string path that is not on the reviewed list"]
    );
    let username = r#"{"@type":"user","usernames":{"@type":"usernames","active_usernames":["x"]}}"#;
    assert_eq!(
        problems(username),
        vec![
            "a string path that is not on the reviewed list",
            "a username that is not synthetic"
        ]
    );
    let real_date = r#"{"@type":"updateUserStatus","user_id":1,"status":{"@type":"userStatusOnline","expires":1792154532}}"#;
    assert_eq!(
        problems(real_date),
        vec!["a date outside the synthetic window"]
    );
    let synthetic_date = r#"{"@type":"updateUserStatus","user_id":1,"status":{"@type":"userStatusOnline","expires":1700000032}}"#;
    assert_eq!(problems(synthetic_date), Vec::<&str>::new());
    let real_option = r#"{"@type":"updateOption","name":"unix_time","value":{"@type":"optionValueInteger","value":"1792154500"}}"#;
    assert_eq!(
        problems(real_option),
        vec!["a date outside the synthetic window"]
    );
    let offset = r#"{"@type":"updateOption","name":"utc_time_offset","value":{"@type":"optionValueInteger","value":"10800"}}"#;
    assert_eq!(problems(offset), vec!["a UTC offset that is not 0"]);
    let id = r#"{"@type":"updateOption","name":"anti_spam_bot_user_id","value":{"@type":"optionValueInteger","value":"2200583762"}}"#;
    assert_eq!(problems(id), Vec::<&str>::new());
}

/// The rules file is in `ScriptedTd`'s format: the five login rules in order, each `when` a
/// [pointer, value] pair (C3 teaches the parser `when` and `nth`; the `on`/`reply` part parses today).
#[test]
fn the_rules_parse_and_answer_the_login() {
    let lines = json_lines(RULES);
    let ons: Vec<&str> = lines
        .iter()
        .map(|l| l["on"].as_str().expect("an `on`"))
        .collect();
    assert_eq!(
        ons,
        [
            "setAuthenticationPhoneNumber",
            "setAuthenticationPhoneNumber",
            "checkAuthenticationCode",
            "checkAuthenticationCode",
            "getMe",
        ]
    );
    assert_eq!(
        lines[0]["when"],
        serde_json::json!(["/phone_number", OWN_NUMBER])
    );
    assert_eq!(lines[2]["when"], serde_json::json!(["/code", CODE]));
    // The phone's answer is WaitCode, then the request's `ok`.
    assert_eq!(
        lines[0]["reply"][0]["authorization_state"]["@type"],
        "authorizationStateWaitCode"
    );
    assert_eq!(lines[0]["reply"][1]["@type"], "ok");
    // The code's answer holds Ready BEFORE the request's `ok` (TDLib's order) and `my_id`, an int64
    // option, as a JSON STRING (critic M14).
    let code_reply = lines[2]["reply"].as_array().expect("a reply list");
    let ready = code_reply
        .iter()
        .position(|o| o["authorization_state"]["@type"] == "authorizationStateReady")
        .expect("Ready in the code's reply");
    assert_eq!(code_reply.last().expect("an ok")["@type"], "ok");
    assert!(ready < code_reply.len() - 1);
    let my_id = code_reply
        .iter()
        .find(|o| o["@type"] == "updateOption" && o["name"] == "my_id")
        .expect("my_id in the code's reply");
    assert!(
        my_id["value"]["value"].is_string(),
        "int64 options are strings"
    );
    // getMe answers the own user, the synthetic number in digits.
    assert_eq!(lines[4]["reply"][0]["phone_number"], OWN_DIGITS);

    let script = Script::from_fixture(RULES);
    assert_eq!(script.rules.len(), 5);
    assert_eq!(script.rules[4].on, "getMe");
    // C3's parser: the `when` pairs of rules 0 and 2 are parsed, the fallbacks have none, and no
    // rule counts or waits.
    assert_eq!(
        script.rules[0].when,
        Some(("/phone_number".to_owned(), serde_json::json!(OWN_NUMBER)))
    );
    assert_eq!(
        script.rules[2].when,
        Some(("/code".to_owned(), serde_json::json!(CODE)))
    );
    for i in [1, 3, 4] {
        assert_eq!(script.rules[i].when, None, "rule {i} has a `when`");
    }
    assert!(
        script.rules.iter().all(|r| r.nth.is_none() && !r.deferred),
        "a login rule counts or waits"
    );
}
