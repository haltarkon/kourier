//! The offline fake's own tests (TH2, lane plan §5.2), against the REAL patched libthreema client
//! where a client is involved. No runtime and no `unshare`: libthreema's protocol code is sans-I/O
//! and the fake has no socket, so both run in this thread. Each test fails with a sentence and
//! never prints a key, an ID or a text.

mod connector;
mod crypto;
mod csp;
pub(crate) mod d2m;
mod directory;
mod envelope;
mod envelope_reflect;
mod joiner;
mod leader;
mod peer;
mod piped;
mod reflect;
mod relay;
mod txn;
mod world;

/// The bytes of a hex string (test data only).
pub fn unhex(text: &str) -> Vec<u8> {
    assert!(text.len().is_multiple_of(2), "a hex string of odd length");
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).expect("hex digits"))
        .collect()
}

/// [`unhex`] into 32 bytes.
pub fn unhex32(text: &str) -> [u8; 32] {
    unhex(text).try_into().expect("32 bytes of hex")
}

/// The files of this directory whose tests drive the fake's servers: the sweep reads them at run
/// time and checks each test (G12).
const SWEPT: &[&str] = &[
    "connector.rs",
    "csp.rs",
    "d2m.rs",
    "directory.rs",
    "envelope_reflect.rs",
    "joiner.rs",
    "leader.rs",
    "peer.rs",
    "piped.rs",
    "reflect.rs",
    "relay.rs",
    "txn.rs",
];

/// The files of this directory that are not swept, each with its reason. A file of `tests/mock/` in
/// neither list fails the sweep: whoever adds one decides which it is.
const NOT_SWEPT: &[(&str, &str)] = &[
    ("mod.rs", "this file: the sweep itself and the hex helpers"),
    (
        "crypto.rs",
        "tests the fake's crypto alone (no server, no client)",
    ),
    (
        "envelope.rs",
        "tests the phone's builders alone (bytes and decoded protobuf, no server, no client)",
    ),
    (
        "world.rs",
        "tests the world's own parts alone (no server, no client)",
    ),
];

/// Moves `i` past the string, character or comment literal that starts at `i` (if one does).
fn skip_literal(text: &[u8], i: usize) -> usize {
    match text[i] {
        b'"' => {
            let mut j = i + 1;
            while j < text.len() && text[j] != b'"' {
                j += if text[j] == b'\\' { 2 } else { 1 };
            }
            j + 1
        }
        b'\'' => {
            // A character literal ('x', '\n', '\'') has its closing quote 2-3 bytes on; a lifetime
            // has none.
            if text.get(i + 1) == Some(&b'\\') {
                let mut j = i + 2;
                while j < text.len() && text[j] != b'\'' {
                    j += 1;
                }
                j + 1
            } else if text.get(i + 2) == Some(&b'\'') {
                i + 3
            } else {
                i + 1
            }
        }
        b'/' if text.get(i + 1) == Some(&b'/') => {
            let mut j = i;
            while j < text.len() && text[j] != b'\n' {
                j += 1;
            }
            j
        }
        b'/' if text.get(i + 1) == Some(&b'*') => {
            let mut j = i + 2;
            while j + 1 < text.len() && !(text[j] == b'*' && text[j + 1] == b'/') {
                j += 1;
            }
            j + 2
        }
        _ => i + 1,
    }
}

/// The body of the function that starts at `from` (between its braces), found by matching braces
/// (literals and comments skipped), and the offset just after the closing brace.
fn function_body(source: &str, from: usize) -> Option<(&str, usize)> {
    let bytes = source.as_bytes();
    let open = from + source[from..].find('{')?;
    let mut depth = 0usize;
    let mut i = open;
    while i < bytes.len() {
        match bytes[i] {
            b'{' => {
                depth += 1;
                i += 1;
            }
            b'}' => {
                depth -= 1;
                i += 1;
                if depth == 0 {
                    return Some((&source[open + 1..i - 1], i));
                }
            }
            _ => i = skip_literal(bytes, i),
        }
    }
    None
}

/// The last statement of a block's text: after the last `;` outside any bracket, or after the last
/// block-like statement (`for`, `if`, `match` ... `}` not followed by `else`).
fn last_statement(body: &str) -> &str {
    let bytes = body.as_bytes();
    // `start` is where the statement under way began, `done` where the last finished one began.
    let (mut depth, mut start, mut done, mut i) = (0usize, 0usize, 0usize, 0usize);
    while i < bytes.len() {
        match bytes[i] {
            b'(' | b'[' | b'{' => {
                depth += 1;
                i += 1;
            }
            b')' | b']' => {
                depth = depth.saturating_sub(1);
                i += 1;
            }
            b'}' => {
                depth = depth.saturating_sub(1);
                i += 1;
                let rest = body[i..].trim_start();
                if depth == 0
                    && !rest.starts_with("else")
                    && !rest.starts_with('.')
                    && !rest.starts_with(';')
                {
                    done = start;
                    start = i;
                }
            }
            b';' if depth == 0 => {
                i += 1;
                done = start;
                start = i;
            }
            _ => i = skip_literal(bytes, i),
        }
    }
    // The text after the last `;` or block is the statement under way, unless nothing follows
    // (a body that ends with its `;`): then the last finished one is the last.
    let last = body[start..].trim();
    if last.is_empty() {
        body[done..start].trim()
    } else {
        last
    }
}

/// The names of the tests of `source` (`#[test]` and `#[tokio::test(..)]` functions) whose LAST
/// statement neither asserts the violations empty nor compares their rules: the end of every mock
/// test (S-mock §9). A check in the middle (the violations after it would go unseen), in a helper
/// after the test, or in a comment does not count.
fn tests_without_a_final_violations_check(source: &str) -> Vec<String> {
    let mut starts: Vec<usize> = source
        .match_indices("#[test]")
        .chain(source.match_indices("#[tokio::test"))
        .map(|(i, _)| i)
        .collect();
    starts.sort_unstable();
    let mut missing = Vec::new();
    for start in starts {
        let Some(at) = source[start..].find("fn ") else {
            missing.push("?".to_owned());
            continue;
        };
        let after = &source[start + at + 3..];
        let name = after.split('(').next().unwrap_or("?").trim().to_owned();
        let body = function_body(source, start + at).map_or("", |(body, _)| body);
        let last = last_statement(body);
        // A check under a condition, or one whose result is thrown away, is not a check.
        let conditional = ["if ", "if(", "match ", "let "]
            .iter()
            .any(|head| last.starts_with(head));
        // TH5a A3: the CHILD of a piped pair (`piped.rs`) holds no world (the fake lives in the
        // parent, whose test ends with the check): it ends with the actor's `finish*()`.
        let checked = if body.contains("offline::in_child()") {
            last.contains(".finish")
        } else {
            last.contains(".assert_empty()") || last.contains("violations().rules()")
        };
        if conditional || !checked {
            missing.push(name);
        }
    }
    missing
}

/// The number of tests in `source`.
fn test_count(source: &str) -> usize {
    source.matches("#[test]").count() + source.matches("#[tokio::test").count()
}

/// The sweep: every test that drives the fake ends by checking what the client broke, and every file
/// of this directory is swept or named as not swept.
#[test]
fn every_server_test_checks_the_violations() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/mock");
    let mut unknown = Vec::new();
    let mut seen = Vec::new();
    for entry in std::fs::read_dir(&dir).expect("tests/mock/") {
        let name = entry
            .expect("a directory entry")
            .file_name()
            .to_string_lossy()
            .into_owned();
        if SWEPT.contains(&name.as_str()) {
            seen.push(name);
        } else if !NOT_SWEPT.iter().any(|(n, _)| *n == name) {
            unknown.push(name);
        }
    }
    assert!(
        unknown.is_empty(),
        "tests/mock/ holds files the sweep knows nothing about: {unknown:?} (add each to SWEPT or NOT_SWEPT)"
    );
    seen.sort();
    assert!(
        seen == SWEPT,
        "a swept file is missing from tests/mock/: found {seen:?}"
    );
    let (mut missing, mut tests) = (Vec::new(), 0);
    for file in SWEPT {
        let source = std::fs::read_to_string(dir.join(file)).expect("a swept file");
        tests += test_count(&source);
        for name in tests_without_a_final_violations_check(&source) {
            missing.push(format!("{file}: {name}"));
        }
    }
    assert!(tests >= 60, "the sweep found too few tests ({tests})");
    assert!(
        missing.is_empty(),
        "tests that do not END with a check of the fake's violations: {missing:?}"
    );
}

/// The sweep refuses what it is for: a test without the check, a tokio test too, a check that is
/// not the last statement, a check in a helper after the test, a check under an `if` or thrown away
/// with `let _`; and it takes what it should: a
/// check at the end, in an `assert_eq!` over several lines, as the last statement of a loop.
#[test]
fn the_sweep_sees_a_test_without_the_final_check() {
    let source = "#[test]\nfn a() {\n    w.violations().assert_empty();\n}\n\
        #[tokio::test(start_paused = true)]\nasync fn b() {\n    run().await;\n}\n\
        #[test]\nfn c() {\n    assert_eq!(\n        w.violations().rules(),\n        vec![\"x;{\"],\n    );\n}\n\
        #[test]\nfn d() {}\n\
        #[test]\nfn e() {\n    w.violations().assert_empty();\n    more(&w);\n}\n\
        #[test]\nfn f() {\n    for n in [1, 2] {\n        run(n);\n        w.violations().assert_empty();\n    }\n}\n\
        #[test]\nfn g() {\n    run();\n}\n\
        fn helper(w: &W) {\n    w.violations().assert_empty();\n}\n\
        #[test]\nfn h() {\n    // w.violations().assert_empty();\n    let c = ';';\n    run(c);\n}\n\
        #[test]\nfn i() {\n    run();\n    if c {\n        w.violations().assert_empty();\n    }\n}\n\
        #[test]\nfn j() {\n    run();\n    let _ = w.violations().rules();\n}\n\
        #[tokio::test]\nasync fn k() {\n    if !support::offline::in_child() {\n        return;\n    }\n    c.finish().await;\n}\n\
        #[tokio::test]\nasync fn l() {\n    if !support::offline::in_child() {\n        return;\n    }\n    w.violations().assert_empty();\n}\n";
    assert_eq!(
        tests_without_a_final_violations_check(source),
        vec!["b", "d", "e", "g", "h", "i", "j", "l"]
    );
}

/// The tests of `tests/connection/` (TH3, call C13) that neither are a parent of the offline child
/// (their body starts the child) nor END with the harness's `finish` (which checks the fake's
/// violations as its own last statement) or the violations check itself.
fn connection_tests_without_a_final_check(source: &str) -> Vec<String> {
    let mut starts: Vec<usize> = source
        .match_indices("#[test]")
        .chain(source.match_indices("#[tokio::test"))
        .map(|(i, _)| i)
        .collect();
    starts.sort_unstable();
    let mut missing = Vec::new();
    for start in starts {
        let Some(at) = source[start..].find("fn ") else {
            missing.push("?".to_owned());
            continue;
        };
        let name = source[start + at + 3..]
            .split('(')
            .next()
            .unwrap_or("?")
            .trim()
            .to_owned();
        let Some((body, _)) = function_body(source, start + at) else {
            missing.push(name);
            continue;
        };
        if body.contains("offline::run_child(") {
            continue;
        }
        let last = last_statement(body);
        let conditional = ["if ", "if(", "match ", "let "]
            .iter()
            .any(|head| last.starts_with(head));
        if conditional
            || !(last.contains(".finish")
                || last.contains(".assert_empty()")
                || last.contains("violations().rules()"))
        {
            missing.push(name);
        }
    }
    missing
}

/// C13: every actor test of `tests/connection/` ends with the harness's `finish` (the fake's
/// violations are its last check); `mod.rs` and `harness.rs` hold no test of their own.
#[test]
fn every_connection_test_ends_with_the_violations_check() {
    actor_tests_end_with_the_violations_check("tests/connection", 10);
}

/// TH4 J3: the same for the join tests of `tests/join/`.
#[test]
fn every_join_test_ends_with_the_violations_check() {
    actor_tests_end_with_the_violations_check("tests/join", 10);
}

/// Every test of the actor tests' directory `dir` (relative to the crate root) ends with its
/// harness's `finish*` or the violations check; at least `least` tests are found.
fn actor_tests_end_with_the_violations_check(dir: &str, least: usize) {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(dir);
    let (mut missing, mut tests) = (Vec::new(), 0);
    for entry in std::fs::read_dir(&dir).expect("the actor tests' directory") {
        let path = entry.expect("a directory entry").path();
        let source = std::fs::read_to_string(&path).expect("a test file");
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if name == "mod.rs" || name == "harness.rs" {
            assert_eq!(test_count(&source), 0, "{name} holds a test");
            continue;
        }
        tests += test_count(&source);
        for test in connection_tests_without_a_final_check(&source) {
            missing.push(format!("{name}: {test}"));
        }
    }
    assert!(tests >= least, "the sweep found too few tests ({tests})");
    assert!(
        missing.is_empty(),
        "actor tests that do not END with the harness's finish: {missing:?}"
    );
}

#[test]
fn the_connection_sweep_sees_a_child_without_finish() {
    let source = "#[test]\nfn p() {\n    support::offline::run_child(module_path!(), \"c\");\n}\n\
        #[tokio::test]\nasync fn c() {\n    if !in_child() {\n        return;\n    }\n    h.finish().await;\n}\n\
        #[tokio::test]\nasync fn d() {\n    h.finish().await;\n    more();\n}\n\
        #[tokio::test]\nasync fn e() {\n    run();\n}\n";
    assert_eq!(
        connection_tests_without_a_final_check(source),
        vec!["d", "e"]
    );
}
