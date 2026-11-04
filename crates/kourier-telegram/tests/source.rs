//! The source checks of the Telegram lane (TG-1 plan §2.10 (3), T-a11, T-b9; critic L2): names are
//! matched as WHOLE names on code with its comment lines left out (prose may name what code must
//! not), and every name this file looks for is assembled at run time, so the file holds none.
//!
//! Scanned: `kourier-telegram/{src,tests}`, `kourier-tdlib/{src,tests}`, `kourier-app/src`,
//! `xtask/src`, the fake DC's test-only workspace `kourier-telegram/fakedc/src` (TD-4a), and the
//! `build.rs` of those crates (the TDLib entry names: every crate's `src`, `tests` and `build.rs`).
//!
//! What these checks cannot see (a reader of this file should know): an IPv4 address written as one
//! `u32` (`Ipv4Addr::from(0x...)` or a decimal), an address or a host name built from pieces at
//! run time, and a process started by a crate outside the two lane crates. They are a net for the
//! common spellings and for a careless edit; the gate of TG-1b, the lock-down and the offline test
//! of the real library are the walls.

use std::path::{Path, PathBuf};

/// The workspace root.
fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Every `.rs` file under `dir` (relative to the root), as (relative path, text).
fn rust_files(dir: &str) -> Vec<(String, String)> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<(String, String)>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries {
            let path = entry.expect("a directory entry").path();
            if path.is_dir() {
                walk(base, &path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                let rel = path
                    .strip_prefix(base)
                    .expect("under the root")
                    .to_string_lossy()
                    .into_owned();
                let text = std::fs::read_to_string(&path).expect("a readable source file");
                out.push((rel, text));
            }
        }
    }
    let base = root().canonicalize().expect("the workspace root");
    let mut out = Vec::new();
    walk(&base, &base.join(dir), &mut out);
    assert!(!out.is_empty(), "nothing to scan under {dir}");
    out
}

/// The `build.rs` of each of these crate directories (relative to the root) that has one.
fn build_scripts(crate_dirs: &[&str]) -> Vec<(String, String)> {
    crate_dirs
        .iter()
        .filter_map(|dir| {
            let rel = format!("{dir}/build.rs");
            let text = std::fs::read_to_string(root().join(&rel)).ok()?;
            Some((rel, text))
        })
        .collect()
}

fn lane_files() -> Vec<(String, String)> {
    let mut all = Vec::new();
    for dir in [
        "crates/kourier-telegram/src",
        "crates/kourier-telegram/tests",
        "crates/kourier-tdlib/src",
        "crates/kourier-tdlib/tests",
        "crates/kourier-app/src",
        "xtask/src",
        // The fake DC's test-only workspace (TD-4a): no Telegram address or host, no process start,
        // no API literal there either.
        "crates/kourier-telegram/fakedc/src",
    ] {
        all.extend(rust_files(dir));
    }
    all.extend(build_scripts(&[
        "crates/kourier-telegram",
        "crates/kourier-tdlib",
        "crates/kourier-app",
        "xtask",
    ]));
    all
}

/// The fake DC (TD-4a) is a workspace of its own below `kourier-telegram`: its sources are scanned
/// like the lane's.
#[test]
fn the_fake_dc_is_scanned() {
    let files = lane_files();
    for module in ["main.rs", "session.rs", "rpc.rs", "tlb.rs"] {
        let path = format!("crates/kourier-telegram/fakedc/src/{module}");
        assert!(
            files.iter().any(|(p, _)| *p == path),
            "{path} is not scanned"
        );
    }
}

/// Every crate's `src` and `tests`, and xtask.
fn workspace_files() -> Vec<(String, String)> {
    let mut all = rust_files("xtask/src");
    all.extend(build_scripts(&["xtask"]));
    let crates = root().join("crates");
    for entry in std::fs::read_dir(&crates).expect("crates/") {
        let name = entry
            .expect("entry")
            .file_name()
            .to_string_lossy()
            .into_owned();
        for sub in ["src", "tests"] {
            if crates.join(&name).join(sub).is_dir() {
                all.extend(rust_files(&format!("crates/{name}/{sub}")));
            }
        }
        all.extend(build_scripts(&[&format!("crates/{name}")]));
    }
    all
}

/// The text without its comment lines (`//`, `///`, `//!`).
fn code(text: &str) -> String {
    text.lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The text without any white space (for the spellings of an address in code).
fn squeezed(text: &str) -> String {
    text.chars().filter(|c| !c.is_whitespace()).collect()
}

/// Whether `text` holds `needle` as a whole name: an identifier character does not touch it where
/// the needle begins or ends with one (WhatsApp's `builder.rs` matcher).
fn names(text: &str, needle: &str) -> bool {
    !name_positions(text, needle).is_empty()
}

/// Where `text` holds `needle` as a whole name (see [`names`]).
fn name_positions(text: &str, needle: &str) -> Vec<usize> {
    let ident = |c: char| c.is_alphanumeric() || c == '_';
    let first = needle.chars().next().is_some_and(ident);
    let last = needle.chars().next_back().is_some_and(ident);
    text.match_indices(needle)
        .filter(|&(at, _)| {
            let before = text[..at].chars().next_back();
            let after = text[at + needle.len()..].chars().next();
            (!first || !before.is_some_and(ident)) && (!last || !after.is_some_and(ident))
        })
        .map(|(at, _)| at)
        .collect()
}

// ----- Telegram's addresses -----

/// Files that may name Telegram's addresses (TG-1b's gate ranges).
const ADDRESS_FILES: &[&str] = &["crates/kourier-telegram/src/gate/ranges.rs"];

/// Telegram's address prefixes as text, and their spellings in code (white space removed).
fn address_patterns() -> Vec<String> {
    let parts: &[&[&str]] = &[
        &["149", ".154."],
        &["91", ".108."],
        &["91", ".105."],
        &["95", ".161."],
        &["185", ".76.151."],
        &["2001", ":b28:"],
        &["2001", ":0b28:"],
        &["2001", ":67c:"],
        &["2001", ":067c:"],
        &["2a0a", ":f280:"],
        &["Ipv4Addr::new", "(149,"],
        &["Ipv4Addr::new", "(91,"],
        &["Ipv4Addr::new", "(95,"],
        &["Ipv4Addr::new", "(185,"],
        &["ipv4", "(149,"],
        &["ipv4", "(91,"],
        &["[149", ",154,"],
        &["[91", ",108,"],
        &["0x2001", ",0xb28"],
        &["0x2001", ",0x67c"],
        &["0x2a0a", ",0xf280"],
        // The same integers with a leading zero (fix F3).
        &["0x2001", ",0x0b28"],
        &["0x2001", ",0x067c"],
    ];
    parts.iter().map(|p| p.concat()).collect()
}

/// The address patterns `text` holds (case-insensitive, comments included, white space removed).
fn address_hits(text: &str) -> Vec<String> {
    let text = squeezed(text).to_ascii_lowercase();
    address_patterns()
        .into_iter()
        .filter(|p| text.contains(&p.to_ascii_lowercase()))
        .collect()
}

#[test]
fn no_file_names_a_telegram_address_outside_the_allow_list() {
    let mut bad = Vec::new();
    for (path, text) in lane_files() {
        if ADDRESS_FILES.contains(&path.as_str()) {
            continue;
        }
        for hit in address_hits(&text) {
            bad.push(format!("{path} names a Telegram address ({hit})"));
        }
    }
    assert!(bad.is_empty(), "{bad:#?}");
}

/// One sample per pattern, written independently of the list: a pattern that is weakened or
/// deleted lets its sample through.
#[test]
fn every_address_pattern_catches_its_sample() {
    let samples: &[&[&str]] = &[
        &["connect 149", ".154.167.50"],
        &["91", ".108.56.1"],
        &["91", ".105.192.100"],
        &["95", ".161.76.100"],
        &["185", ".76.151.1"],
        &["2001", ":b28:f23d:f001::a"],
        &["2001", ":0b28:f23d::a"],
        &["2001", ":67c:4e8:f002::a"],
        &["2001", ":067c:4e8::a"],
        &["2a0a", ":f280:0203::a"],
        &["Ipv4Addr::new", "( 149, 154, 167, 51)"],
        &["Ipv4Addr::new", "(91, 108, 56, 1)"],
        &["Ipv4Addr::new", "(95, 161, 76, 100)"],
        &["Ipv4Addr::new", "(185, 76, 151, 1)"],
        &["ipv4", "(149, 154, 0, 0)"],
        &["ipv4", "(91, 105, 0, 0)"],
        &["[149", ", 154, 167, 50]"],
        &["[91", ", 108, 4, 1]"],
        &["0x2001", ", 0xb28, 0xf23d"],
        &["0x2001", ", 0x67c, 0x4e8"],
        &["0x2a0a", ", 0xf280, 0x203"],
        &["0x2001", ", 0x0b28, 0xf23d"],
        &["0x2001", ", 0x067c, 0x4e8"],
    ];
    assert_eq!(
        samples.len(),
        address_patterns().len(),
        "a pattern without its own sample"
    );
    for (n, sample) in samples.iter().enumerate() {
        let hits = address_hits(&sample.concat());
        assert!(
            hits.contains(&address_patterns()[n]),
            "address pattern {n} no longer catches its sample"
        );
    }
    assert!(
        address_hits("let port = 9150; // 149 sheep, 91 goats").is_empty(),
        "a harmless line is taken for an address"
    );
}

// ----- Telegram's host names (fix F3) -----

/// The host names nothing may contact: as WHOLE names in code (comment lines stay free to say them;
/// the docs are not scanned), outside the one file of the gate's ranges.
fn host_patterns() -> Vec<String> {
    [
        ["tele", "gram.org"].concat(),
        ["t", ".me"].concat(),
        ["tele", "gram.me"].concat(),
        ["telegra", ".ph"].concat(),
    ]
    .to_vec()
}

fn host_hits(text: &str) -> Vec<String> {
    let code = code(text);
    host_patterns()
        .into_iter()
        .filter(|p| names(&code, p))
        .collect()
}

#[test]
fn no_file_names_a_telegram_host_outside_the_allow_list() {
    let mut bad = Vec::new();
    for (path, text) in lane_files() {
        if ADDRESS_FILES.contains(&path.as_str()) {
            continue;
        }
        for hit in host_hits(&text) {
            bad.push(format!("{path} names a Telegram host ({hit})"));
        }
    }
    assert!(bad.is_empty(), "{bad:#?}");
}

/// One sample per host pattern, written independently of the list; harmless lines pass.
#[test]
fn every_host_pattern_catches_its_sample() {
    let samples: [String; 4] = [
        ["let url = \"https://venus.web.tele", "gram.org/x\";"].concat(),
        ["let url = \"https://t", ".me/joinchat\";"].concat(),
        ["const HOST: &str = \"tele", "gram.me\";"].concat(),
        ["fetch(\"https://telegra", ".ph/page\");"].concat(),
    ];
    assert_eq!(
        samples.len(),
        host_patterns().len(),
        "a pattern without its own sample"
    );
    for (n, sample) in samples.iter().enumerate() {
        assert!(
            host_hits(sample).contains(&host_patterns()[n]),
            "host pattern {n} no longer catches its sample"
        );
    }
    // Prose in a comment, a longer name and ordinary identifiers are not hosts.
    for harmless in [
        ["// see t", ".me for the story"].concat(),
        "let x = event.metadata().level(); let y = b.newest.message;".to_owned(),
        ["let url = \"https://nottele", "gram.org/\";"].concat(),
        "let layout = layout.metainfo;".to_owned(),
    ] {
        assert!(
            host_hits(&harmless).is_empty(),
            "taken for a host: {harmless}"
        );
    }
}

// ----- processes -----

/// The one file of the lane that may start a process: the helper spawner (decisions D3 H4).
const SPAWN_FILES: &[&str] = &["crates/kourier-tdlib/src/spawn.rs"];

/// A test may start a process only by re-executing its own binary.
fn reexecutes_itself(squeezed_code: &str, at: usize) -> bool {
    squeezed_code[at..].starts_with(&["Comm", "and::new(", "std::env::current_exe()"].concat())
}

/// The libc entry points that start a process (fix F3). `system` is left out on purpose: it is not
/// a name this lane's code could use without `unsafe` and a C string, and `libc::system` is caught
/// by the plain `libc::` list below only for the names listed here.
fn libc_starts() -> Vec<String> {
    [
        "fork",
        "vfork",
        "execv",
        "execve",
        "execvp",
        "posix_spawn",
        "clone",
    ]
    .iter()
    .map(|n| (*n).to_owned())
    .collect()
}

/// The identifiers inside the first `{...}` after each `prefix` in `plain` (a `use` list).
fn use_list_names(plain: &str, prefix: &str) -> Vec<String> {
    let ident = |c: char| c.is_alphanumeric() || c == '_';
    let mut out = Vec::new();
    for (at, _) in plain.match_indices(prefix) {
        let rest = &plain[at + prefix.len()..];
        let Some(end) = rest.find('}') else { continue };
        out.extend(
            rest[..end]
                .split(|c: char| !ident(c))
                .filter(|t| !t.is_empty())
                .map(str::to_owned),
        );
    }
    out
}

fn process_violations(path: &str, text: &str) -> Vec<String> {
    if SPAWN_FILES.contains(&path) {
        return Vec::new();
    }
    let lane =
        path.starts_with("crates/kourier-telegram/") || path.starts_with("crates/kourier-tdlib/");
    if !lane {
        return Vec::new();
    }
    let test = path.contains("/tests/");
    let plain = code(text);
    let code = squeezed(&plain);
    let mut bad = Vec::new();
    let new = ["Command", "::new("].concat();
    for (at, _) in code.match_indices(&new) {
        if !(test && reexecutes_itself(&code, at)) {
            bad.push(format!("{path} starts a process"));
        }
    }
    for name in [
        ["process", "::Command"].concat(),
        ["Command", "Ext"].concat(),
        ["pre_", "exec"].concat(),
    ] {
        if !test && names(&code, &name) {
            bad.push(format!("{path} names {name}"));
        }
    }
    // A `use std::process::{.., Command, ..}` list names it without `process::Command` (not in a
    // test, which may import it to re-execute itself); a rename defeats `Command::new(` for
    // everyone, a test included.
    let command = ["Com", "mand"].concat();
    let process_list = ["process", "::{"].concat();
    if !test && use_list_names(&plain, &process_list).contains(&command) {
        bad.push(format!("{path} imports {command} in a process list"));
    }
    if names(&plain, &format!("{command} as ")) {
        bad.push(format!("{path} renames {command}"));
    }
    // The libc entry points that start a process, named in full or imported from a `libc::{..}`
    // list, in every file of the lane (a test included).
    let libc_list = ["li", "bc::{"].concat();
    let listed = use_list_names(&plain, &libc_list);
    for name in libc_starts() {
        if names(&code, &format!("libc::{name}")) || listed.contains(&name) {
            bad.push(format!("{path} names libc {name}"));
        }
    }
    bad
}

#[test]
fn only_the_spawner_starts_a_process() {
    let mut bad = Vec::new();
    for (path, text) in lane_files() {
        bad.extend(process_violations(&path, &text));
    }
    assert!(bad.is_empty(), "{bad:#?}");
    // The check sees a spawn in the actor, and lets a test re-execute itself.
    let spawn = ["let c = std::process::Comm", "and::new(\"x\");"].concat();
    assert!(!process_violations("crates/kourier-telegram/src/actor.rs", &spawn).is_empty());
    let child = [
        "std::process::",
        "Command::new(std::env::current_exe().unwrap())",
    ]
    .concat();
    assert!(process_violations("crates/kourier-telegram/tests/x.rs", &child).is_empty());
}

/// Fix F3: one sample per added spelling, each seen where it must be and not seen where it may be.
#[test]
fn the_process_spellings_added_by_the_review_are_seen() {
    let src = "crates/kourier-telegram/src/actor.rs";
    let test = "crates/kourier-tdlib/tests/x.rs";
    // `process::{..}` lists and renames.
    let list = ["use std::process::{Child, Com", "mand};"].concat();
    let rename_list = ["use std::process::{Child, Com", "mand as Cmd};"].concat();
    let rename = ["use std::process::Com", "mand as Cmd;"].concat();
    for sample in [&list, &rename_list, &rename] {
        assert!(
            !process_violations(src, sample).is_empty(),
            "a spawner import passed in src: {sample}"
        );
    }
    assert!(
        process_violations(test, &list).is_empty(),
        "a test may import the type to re-execute itself"
    );
    for sample in [&rename_list, &rename] {
        assert!(
            !process_violations(test, sample).is_empty(),
            "a rename passed in a test: {sample}"
        );
    }
    // libc entry points, in full and from a list, in src and in tests.
    for name in libc_starts() {
        let direct = format!("unsafe {{ libc::{name}(); }}");
        let listed = format!("use libc::{{c_int, {name} as other}};");
        for path in [src, test] {
            assert!(
                !process_violations(path, &direct).is_empty(),
                "libc {name} passed in {path}"
            );
            assert!(
                !process_violations(path, &listed).is_empty(),
                "a libc list with {name} passed in {path}"
            );
        }
    }
    // What is not a process start passes: the `clone` method, other libc calls, the spawner itself.
    for fine in [
        "let a = b.clone(); libc::close_range(4, !0, 4);",
        "use libc::{c_int, dup2};",
        "libc::forkpty_is_not_fork();",
    ] {
        assert!(
            process_violations(src, fine).is_empty(),
            "taken for a process start: {fine}"
        );
    }
    let commented = ["// libc::", "fork is named in a comment"].concat();
    assert!(process_violations(src, &commented).is_empty());
    assert!(process_violations(SPAWN_FILES[0], &list).is_empty());
}

// ----- TDLib's entry points (T-a11) -----

/// The files that may name TDLib's entry points: the one loader and its test.
const TDJSON_FILES: &[&str] = &[
    "crates/kourier-tdlib/src/dlopen.rs",
    "crates/kourier-tdlib/tests/dlopen.rs",
];

fn tdjson_names() -> Vec<String> {
    let td: &[&str] = &[
        "send",
        "receive",
        "execute",
        "create_client_id",
        "json_client_create",
        "json_client_send",
        "json_client_receive",
        "json_client_execute",
        "json_client_destroy",
        "set_log_message_callback",
        "set_log_fatal_error_callback",
        "set_log_file_path",
        "set_log_verbosity_level",
        "set_log_max_file_size",
    ];
    td.iter().map(|n| ["td", "_", n].concat()).collect()
}

/// Whether `flat` (white space removed) holds a `link(..)` attribute that names the TDLib library
/// among its arguments, in any order (`kind = "dylib", name = "tdjson"`), or a build script line that
/// asks cargo or the linker for it (fix F3).
fn links_tdjson(flat: &str) -> bool {
    let name = ["name=\"td", "json\""].concat();
    let linked = flat.match_indices("link(").any(|(at, _)| {
        let args = &flat[at..];
        let end = args.find(')').unwrap_or(args.len());
        args[..end].contains(&name)
    });
    let lib = ["td", "json"].concat();
    let build_line = flat.match_indices("rustc-link-lib").any(|(at, _)| {
        flat[at..]
            .chars()
            .take(48)
            .collect::<String>()
            .contains(&lib)
    });
    linked || build_line || names(flat, &format!("-l{lib}"))
}

fn tdjson_violations(path: &str, text: &str) -> Vec<String> {
    if TDJSON_FILES.contains(&path) {
        return Vec::new();
    }
    let code = code(text);
    let flat = squeezed(&code);
    let mut bad: Vec<String> = tdjson_names()
        .into_iter()
        .filter(|n| names(&code, n))
        .map(|n| format!("{path} names {n}"))
        .collect();
    for call in [
        ["dl", "open("].concat(),
        ["dl", "mopen("].concat(),
        ["dl", "sym("].concat(),
    ] {
        if names(&flat, &call) {
            bad.push(format!("{path} calls {call}"));
        }
    }
    if links_tdjson(&flat) {
        bad.push(format!("{path} links the TDLib library"));
    }
    bad
}

#[test]
fn only_dlopen_rs_names_tdjson() {
    let mut bad = Vec::new();
    for (path, text) in workspace_files() {
        bad.extend(tdjson_violations(&path, &text));
    }
    assert!(bad.is_empty(), "{bad:#?}");
    let named = ["const X: &str = \"td", "_receive\";"].concat();
    assert!(!tdjson_violations("crates/kourier-telegram/src/link.rs", &named).is_empty());
    let prose = ["// the one td", "_receive caller, after dl", "open()"].concat();
    assert!(tdjson_violations("crates/kourier-telegram/src/link.rs", &prose).is_empty());
}

/// Fix F3: the link spellings, each with its own sample; a harmless attribute passes.
#[test]
fn every_way_to_link_the_tdlib_library_is_seen() {
    let attr = |args: &str| format!("#[link({args})]\nextern \"C\" {{}}");
    let lib = ["td", "json"].concat();
    let samples = [
        attr(&format!("name = \"{lib}\"")),
        attr(&format!("kind = \"dylib\", name = \"{lib}\"")),
        attr(&format!("name = \"{lib}\", kind = \"dylib\"")),
        format!("println!(\"cargo:rustc-link-lib={lib}\");"),
        format!("println!(\"cargo:rustc-link-lib=dylib={lib}\");"),
        format!("println!(\"cargo:rustc-link-arg=-l{lib}\");"),
    ];
    for sample in &samples {
        for path in [
            "crates/kourier-telegram/src/link.rs",
            "crates/kourier-telegram/build.rs",
            "crates/kourier-qt/build.rs",
        ] {
            assert!(
                !tdjson_violations(path, sample).is_empty(),
                "{path} links the library unseen: {sample}"
            );
        }
        // The loader's own files are the one place that may name it.
        assert!(tdjson_violations(TDJSON_FILES[0], sample).is_empty());
    }
    for fine in [
        attr("name = \"z\""),
        attr("kind = \"static\", name = \"sqlcipher\""),
        format!("// #[link(name = \"{lib}\")] is only in a comment"),
        format!("let name = \"lib{lib}.so.1\";"),
        "println!(\"cargo:rustc-link-lib=z\");".to_owned(),
    ] {
        assert!(
            tdjson_violations("crates/kourier-telegram/build.rs", &fine).is_empty(),
            "taken for a link of the library: {fine}"
        );
    }
}

// ----- API credentials -----

/// A hex run of exactly 32 digits that is not all zeros (the dummy's): the shape of an `api_hash`.
fn hash_literals(text: &str) -> usize {
    let hex = |c: char| c.is_ascii_hexdigit();
    let word = |c: char| c.is_alphanumeric() || c == '_';
    let chars: Vec<char> = text.chars().collect();
    let mut found = 0;
    let mut i = 0;
    while i < chars.len() {
        if hex(chars[i]) && (i == 0 || !word(chars[i - 1])) {
            let end = (i..chars.len())
                .find(|&j| !hex(chars[j]))
                .unwrap_or(chars.len());
            let bounded = end == chars.len() || !word(chars[end]);
            if bounded && end - i == 32 && chars[i..end].iter().any(|&c| c != '0') {
                found += 1;
            }
            i = end.max(i + 1);
        } else {
            i += 1;
        }
    }
    found
}

/// An `api_id` given a number other than the dummy's 1, as a field, a JSON key or a variable
/// (`api_id`, a colon or an equals sign, then digits; comments included).
fn id_literals(text: &str) -> usize {
    let flat = squeezed(text).to_ascii_lowercase();
    let id = ["api", "_id"].concat();
    flat.match_indices(&id)
        .filter(|(at, _)| {
            let rest = flat[at + id.len()..].trim_start_matches('"');
            let Some(value) = rest.strip_prefix(':').or_else(|| rest.strip_prefix('=')) else {
                return false;
            };
            let digits: String = value
                .trim_start_matches('"')
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            !digits.is_empty() && digits != "1"
        })
        .count()
}

/// An `ApiCredentials { id: <n>` literal with an `n` other than the dummy's 1, and a
/// `parse(Some("<digit>` call that hands the credentials parser a quoted id (fix F3). White space
/// and case do not matter; comments are included, like [`id_literals`].
fn credential_literals(text: &str) -> usize {
    let flat = squeezed(text).to_ascii_lowercase();
    let literal = ["api", "credentials{id:"].concat();
    let fields = flat
        .match_indices(&literal)
        .filter(|(at, _)| {
            let digits: String = flat[at + literal.len()..]
                .chars()
                .take_while(|c| c.is_ascii_digit() || *c == '_')
                .filter(char::is_ascii_digit)
                .collect();
            !digits.is_empty() && digits != "1"
        })
        .count();
    let call = ["parse(some(", "\""].concat();
    let parsed = flat
        .match_indices(&call)
        .filter(|(at, _)| {
            flat[at + call.len()..]
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_digit())
        })
        .count();
    fields + parsed
}

/// Files outside the two lane crates whose 32-digit hex literals are known and harmless, with how
/// many each holds: a test of the log redaction that feeds it a UUID without hyphens. A second
/// literal in such a file, or one in any other file of `kourier-app/src`, fails the check.
const HASH_ALLOWED: &[(&str, usize)] = &[("crates/kourier-app/src/logging.rs", 1)];

/// Where a 32-digit hex literal is refused: the two lane crates and, because the credentials are
/// wired in there (TG-1b), the app's source.
fn hash_scanned(path: &str) -> bool {
    path.starts_with("crates/kourier-telegram/")
        || path.starts_with("crates/kourier-tdlib/")
        || path.starts_with("crates/kourier-app/src/")
}

/// The files that may parse an API pair from text, with the reason (re-check N-1): a test that built
/// a pair of its own would hand a dialing start something other than the dummy pair and slip past the
/// belt of G-3 (a hash-shaped test value is built at run time, so no literal rule sees it).
const FROM_TEXT_ALLOWED: &[(&str, Scope, &str)] = &[
    (
        "crates/kourier-telegram/src/api.rs",
        Scope::WholeFile,
        "the parser itself and its own tests (no start, no gate)",
    ),
    (
        "crates/kourier-app/src/telegram.rs",
        Scope::NonTestCode,
        "the reader of the kept run profile's API file (`read_api_file_as`); its tests go through the reader",
    ),
];

/// How much of an allowed file may name the pair parser (re-check R-3).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Scope {
    /// The whole file, its `#[cfg(test)]` items included (the parser's own tests).
    WholeFile,
    /// The code outside `#[cfg(test)]` items only: a test of the reader goes through the reader and
    /// never builds a pair of its own.
    NonTestCode,
}

/// The name of the pair parser, assembled at run time.
fn from_text_name() -> String {
    ["from", "_text"].concat()
}

/// Whether the file at `path` names the pair parser (`ApiCredentials::from_text`, also through an
/// alias or as a function value) outside the places of [`FROM_TEXT_ALLOWED`]: anywhere in a file that
/// is not listed, in a `#[cfg(test)]` item of a file listed for its non-test code only. Comment lines
/// do not count. A gate the test strip cannot delimit in such a file counts as a hit (it never drops
/// what it cannot delimit).
fn parses_a_pair_outside_its_places(path: &str, text: &str) -> bool {
    let name = from_text_name();
    match FROM_TEXT_ALLOWED.iter().find(|(p, _, _)| *p == path) {
        None => names(&code(text), &name),
        Some((_, Scope::WholeFile, _)) => false,
        Some((_, Scope::NonTestCode, _)) => match non_test_code(text) {
            Ok(plain) => {
                name_positions(&code(text), &name).len() > name_positions(&plain, &name).len()
            }
            Err(_) => true,
        },
    }
}

#[test]
fn no_api_literal_but_the_dummy() {
    let mut bad = Vec::new();
    for (path, text) in lane_files() {
        if parses_a_pair_outside_its_places(&path, &text) {
            bad.push(format!(
                "{path} parses an API pair from text: tests hand the dummy pair only"
            ));
        }
        let scanned = hash_scanned(&path);
        let allowed = HASH_ALLOWED
            .iter()
            .find(|(p, _)| *p == path)
            .map_or(0, |(_, n)| *n);
        if scanned && hash_literals(&text) > allowed {
            bad.push(format!("{path} holds a 32-digit hex literal"));
        }
        if id_literals(&text) > 0 || credential_literals(&text) > 0 {
            bad.push(format!("{path} gives an api id a value"));
        }
    }
    assert!(bad.is_empty(), "{bad:#?}");
    assert!(
        hash_scanned("crates/kourier-app/src/app.rs"),
        "the app is not scanned"
    );
    assert!(hash_scanned("crates/kourier-tdlib/tests/x.rs"));
    assert!(
        !hash_scanned("crates/kourier-core/src/x.rs"),
        "a crate that is not the lane's"
    );
    // Re-check N-1: the pair parser is named only where the table says, and every place still names it.
    for (path, scope, why) in FROM_TEXT_ALLOWED {
        let text = std::fs::read_to_string(root().join(path)).expect("a parser file");
        let place = match scope {
            Scope::WholeFile => code(&text),
            Scope::NonTestCode => non_test_code(&text).expect("a parser file's test strip"),
        };
        assert!(
            names(&place, &from_text_name()),
            "{path} no longer names the pair parser ({why}): a stale entry is a hole"
        );
    }
    let parser = from_text_name();
    let ty = ["Api", "Credentials"].concat();
    let samples = [
        format!("let p = {ty}::{parser}(\"1\", &hash).expect(\"a pair\");"),
        format!("let p = {ty} :: {parser} (id, hash);"),
        format!("let f = {ty}::{parser};"),
        format!("use x::{ty} as Pair;\nlet p = Pair::{parser}(a, b);"),
    ];
    for path in [
        "crates/kourier-telegram/src/real.rs",
        "crates/kourier-telegram/src/supervisor/tests.rs",
        "crates/kourier-telegram/tests/it.rs",
        "crates/kourier-app/src/app.rs",
        "crates/kourier-tdlib/tests/x.rs",
        "xtask/src/tg_run/tests/connect.rs",
    ] {
        for sample in &samples {
            assert!(
                parses_a_pair_outside_its_places(path, sample),
                "{path} may build a pair of its own: {sample}"
            );
        }
        assert!(!parses_a_pair_outside_its_places(
            path,
            &format!("// {}", samples[0])
        ));
        assert!(!parses_a_pair_outside_its_places(
            path,
            "fn a_pair_from_text_is_fine() {}"
        ));
    }
    for (path, _, _) in FROM_TEXT_ALLOWED {
        for sample in &samples {
            assert!(
                !parses_a_pair_outside_its_places(path, sample),
                "{path} is a place that parses a pair"
            );
        }
    }
    // Re-check R-3: the app's reader file is a place in its non-test code only. A `#[cfg(test)]` module
    // under that path that calls the parser is seen, beside the reader's own call (allowed) or alone;
    // the parser's own file keeps its tests.
    let call = format!("{ty}::{parser}(id, hash)");
    let reader = format!("fn read() {{ {call}.ok_or(()) }}\n");
    let gated_module = format!("#[cfg(test)]\nmod tests {{\n    fn f() {{ {call}; }}\n}}\n");
    let gated_fn = format!("#[cfg(test)]\nfn g() {{ let _ = {call}; }}\n");
    let app_file = "crates/kourier-app/src/telegram.rs";
    assert!(
        !parses_a_pair_outside_its_places(app_file, &reader),
        "the reader's own call is allowed"
    );
    for gated in [&gated_module, &gated_fn] {
        assert!(
            parses_a_pair_outside_its_places(app_file, gated),
            "a test item of the app's reader file builds a pair: {gated}"
        );
        assert!(
            parses_a_pair_outside_its_places(app_file, &format!("{reader}{gated}")),
            "a test item beside the reader builds a pair: {gated}"
        );
        assert!(
            !parses_a_pair_outside_its_places("crates/kourier-telegram/src/api.rs", gated),
            "the parser's own tests are allowed"
        );
    }
    // Every allowed file still holds exactly what its entry says (a stale entry is a hole).
    for (path, n) in HASH_ALLOWED {
        let text = std::fs::read_to_string(root().join(path)).expect("an allowed file");
        assert_eq!(
            hash_literals(&text),
            *n,
            "{path} no longer holds {n} literal(s)"
        );
    }
    // The credential literals of fix F3.
    let literal = |id: &str| ["Api", "Credentials { id: ", id, ", .. }"].concat();
    assert_eq!(credential_literals(&literal("94575")), 1, "an id passes");
    assert_eq!(
        credential_literals(&literal("94_575")),
        1,
        "an id with an underscore passes"
    );
    assert_eq!(
        credential_literals(&literal("1")),
        0,
        "the dummy id is refused"
    );
    assert_eq!(
        credential_literals(&literal("1_0")),
        1,
        "1_0 is 10, not the dummy"
    );
    assert_eq!(
        credential_literals(&["Api", "Credentials {\n    id: 77,\n"].concat()),
        1,
        "a literal over several lines passes"
    );
    let parse = |arg: &str| ["Api", "Credentials::parse(Some", arg, ", None)"].concat();
    assert_eq!(
        credential_literals(&parse("(\"94575\")")),
        1,
        "a quoted id passes"
    );
    assert_eq!(
        credential_literals(&parse("(id)")),
        0,
        "a variable is refused"
    );
    assert_eq!(
        credential_literals(&parse("(\"x\")")),
        0,
        "a non-number is no id"
    );
    let zeros = "0".repeat(32);
    assert_eq!(
        hash_literals(&format!("hash: \"{zeros}\"")),
        0,
        "the dummy is refused"
    );
    let hash = "ab".repeat(16);
    assert_eq!(
        hash_literals(&format!("hash: \"{hash}\"")),
        1,
        "a hash passes"
    );
    assert_eq!(
        hash_literals(&format!("{hash}ab {hash}x")),
        0,
        "a longer run is a hash"
    );
    let id = ["api", "_id"].concat();
    assert_eq!(id_literals(&format!("\"{id}\": 94575")), 1, "an id passes");
    assert_eq!(
        id_literals(&format!("{id} = 1;")),
        0,
        "the dummy id is refused"
    );
}

// ----- test support does not ship (TG-1b fix G8) -----

/// The feature that keeps test support out of a release build.
const SUPPORT_FEATURE: &str = "test-support";

/// The items of `text` (found by their declaration line, an `item` prefix at the start of a line,
/// indentation ignored: an item inside a module is found too) that are not behind
/// `#[cfg(feature = "test-support")]`: the attribute must stand in the attribute block directly
/// above the item (attributes and doc comments only between them).
fn ungated(text: &str, items: &[&str]) -> Vec<String> {
    let lines: Vec<&str> = text.lines().collect();
    let cfg = format!("#[cfg(feature = \"{SUPPORT_FEATURE}\")]");
    let mut bad = Vec::new();
    for item in items {
        let found: Vec<usize> = lines
            .iter()
            .enumerate()
            .filter(|(_, l)| {
                l.trim_start().strip_prefix(item).is_some_and(|rest| {
                    !rest.starts_with(|c: char| c.is_alphanumeric() || c == '_')
                })
            })
            .map(|(n, _)| n)
            .collect();
        if found.is_empty() {
            bad.push(format!("{item}: no declaration found"));
        }
        for at in found {
            let gated = lines[..at]
                .iter()
                .rev()
                .take_while(|l| {
                    let t = l.trim_start();
                    t.starts_with("#[") || t.starts_with("///")
                })
                .any(|l| l.trim() == cfg);
            if !gated {
                bad.push(format!(
                    "{item} (line {}) is not behind the feature",
                    at + 1
                ));
            }
        }
    }
    bad
}

/// The lines of a manifest that name the feature outside the two places it may stand: its own
/// definition in `[features]`, and a `[dev-dependencies]` entry (the section of a line is the last
/// `[header]` above it). In particular: never in `[dependencies]`, never in a `default` list, never
/// in another feature's list but `kourier-telegram`'s own definition.
fn support_in_the_wrong_place(manifest: &str) -> Vec<String> {
    let mut section = String::new();
    let mut bad = Vec::new();
    for (n, line) in manifest.lines().enumerate() {
        let t = line.trim();
        if t.starts_with("//") || t.starts_with('#') {
            continue;
        }
        if t.starts_with('[') {
            section = t.to_owned();
            continue;
        }
        if !t.contains(SUPPORT_FEATURE) {
            continue;
        }
        let definition = section == "[features]" && t.starts_with(&format!("{SUPPORT_FEATURE} "));
        let dev = section == "[dev-dependencies]";
        if !(definition || dev) {
            bad.push(format!("line {} ({section}): {t}", n + 1));
        }
    }
    bad
}

#[test]
fn test_support_sits_behind_its_feature_and_only_dev_dependencies_turn_it_on() {
    let read = |rel: &str| std::fs::read_to_string(root().join(rel)).expect("a source file");
    let mut bad = Vec::new();
    // kourier-tdlib: the trap listeners and the offline-child launchers.
    bad.extend(ungated(
        &read("crates/kourier-tdlib/src/lib.rs"),
        &["pub mod traps"],
    ));
    bad.extend(ungated(
        &read("crates/kourier-tdlib/src/spawn.rs"),
        &[
            "pub fn spawn_self",
            "pub const OFFLINE_CHILD_ENV",
            "const OFFLINE_SETUP",
            "pub fn offline_available",
            "pub struct OfflineRun",
            "pub fn run_self_offline",
            "pub fn in_offline_child",
            "pub fn offline_namespace",
            "pub fn helper_socket",
            "pub struct Companion",
            "impl Drop for Companion",
            "pub fn spawn_offline_companion",
        ],
    ));
    // kourier-telegram: the re-export for the app's test, and the two probes of its tripwire (fix
    // G-1: they ask the dialer's permission with no socket; each carries the gate itself).
    bad.extend(ungated(
        &read("crates/kourier-telegram/src/lib.rs"),
        &[
            "pub mod offline",
            "pub fn a_dial_is_permitted",
            "pub fn a_listed_address",
        ],
    ));
    // Every manifest of the workspace.
    let mut manifests = vec!["Cargo.toml".to_owned(), "xtask/Cargo.toml".to_owned()];
    for entry in std::fs::read_dir(root().join("crates")).expect("crates/") {
        let name = entry
            .expect("entry")
            .file_name()
            .to_string_lossy()
            .into_owned();
        manifests.push(format!("crates/{name}/Cargo.toml"));
    }
    let mut named = 0;
    for manifest in manifests {
        let Ok(text) = std::fs::read_to_string(root().join(&manifest)) else {
            continue;
        };
        named += text.matches(SUPPORT_FEATURE).count();
        for line in support_in_the_wrong_place(&text) {
            bad.push(format!("{manifest}: {line}"));
        }
    }
    assert!(
        named > 0,
        "no manifest names the feature: the check scans nothing"
    );
    assert!(
        bad.is_empty(),
        "test support is not kept out of a release build: {bad:#?}"
    );
}

/// The two helpers see what they must: a missing gate, a gate on another item, a feature turned on
/// from `[dependencies]` or from a `default` list.
#[test]
fn the_test_support_checks_see_their_samples() {
    let cfg = format!("#[cfg(feature = \"{SUPPORT_FEATURE}\")]");
    let gated = format!("/// doc\n{cfg}\n#[doc(hidden)]\npub fn one() {{}}\n");
    assert!(ungated(&gated, &["pub fn one"]).is_empty());
    let attribute_between = format!("{cfg}\n/// doc\n#[doc(hidden)]\npub fn one() {{}}\n");
    assert!(ungated(&attribute_between, &["pub fn one"]).is_empty());
    // No gate; a gate on the item above; a gate separated by code; a name that only starts alike.
    assert_eq!(ungated("pub fn one() {}\n", &["pub fn one"]).len(), 1);
    let other = format!("{cfg}\npub fn zero() {{}}\npub fn one() {{}}\n");
    assert_eq!(ungated(&other, &["pub fn one"]).len(), 1);
    let apart = format!("{cfg}\nfn x() {{}}\npub fn one() {{}}\n");
    assert_eq!(ungated(&apart, &["pub fn one"]).len(), 1);
    assert_eq!(ungated("pub fn one_more() {}\n", &["pub fn one"]).len(), 1);
    // An item inside a module is found by its indented declaration, gated or not (fix G-1).
    let nested_gated =
        format!("mod m {{\n    {cfg}\n    #[doc(hidden)]\n    pub fn one() {{}}\n}}\n");
    assert!(ungated(&nested_gated, &["pub fn one"]).is_empty());
    assert_eq!(
        ungated("mod m {\n    pub fn one() {}\n}\n", &["pub fn one"]).len(),
        1,
        "an indented item without its gate passed"
    );
    assert_eq!(
        ungated("", &["pub fn one"]).len(),
        1,
        "a missing item is reported"
    );

    let ok = format!(
        "[features]\n{f} = [\"a/{f}\"]\n[dev-dependencies]\na = {{ features = [\"{f}\"] }}\n",
        f = SUPPORT_FEATURE
    );
    assert!(support_in_the_wrong_place(&ok).is_empty());
    for wrong in [
        format!("[dependencies]\na = {{ features = [\"{SUPPORT_FEATURE}\"] }}\n"),
        format!("[features]\ndefault = [\"{SUPPORT_FEATURE}\"]\n"),
        format!("[features]\nother = [\"a/{SUPPORT_FEATURE}\"]\n"),
        format!("[build-dependencies]\na = {{ features = [\"{SUPPORT_FEATURE}\"] }}\n"),
    ] {
        assert_eq!(support_in_the_wrong_place(&wrong).len(), 1, "{wrong}");
    }
}

// ----- the WhatsApp check's names -----

#[test]
fn no_whatsapp_entry_names() {
    let entry = [
        ["net_", "parts"].concat(),
        ["re", "al::"].concat(),
        ["::", "re", "al"].concat(),
    ];
    let mut bad = Vec::new();
    for (path, text) in lane_files() {
        let lane = path.starts_with("crates/kourier-telegram/")
            || path.starts_with("crates/kourier-tdlib/");
        for name in &entry {
            if lane && names(&text, name) {
                bad.push(format!("{path} names {name}"));
            }
        }
    }
    assert!(bad.is_empty(), "{bad:#?}");
}

// ----- the section of the architecture doc (fix F10) -----

/// Whether a line cites architecture section 7.3 while it speaks of Telegram or TDLib. Section 7.3
/// is the M1 contract; Telegram is section 7.5 (the other networks' lines may cite 7.3). The
/// section number is assembled at run time, so this file does not hold it.
fn cites_the_wrong_section(line: &str) -> bool {
    let wrong = format!("\u{a7}7.{}", 3);
    let lower = line.to_lowercase();
    line.contains(&wrong) && (lower.contains("telegram") || lower.contains("tdlib"))
}

#[test]
fn telegram_lines_cite_section_7_5_of_the_architecture() {
    let mut files = rust_files("crates/kourier-telegram");
    files.extend(rust_files("crates/kourier-tdlib"));
    files.extend(rust_files("crates/kourier-model/src"));
    files.extend(rust_files("xtask/src"));
    for manifest in [
        "crates/kourier-telegram/Cargo.toml",
        "crates/kourier-tdlib/Cargo.toml",
        "crates/kourier-app/Cargo.toml",
    ] {
        let text = std::fs::read_to_string(root().join(manifest)).expect("a manifest");
        files.push((manifest.to_owned(), text));
    }
    let bad: Vec<String> = files
        .iter()
        .flat_map(|(path, text)| {
            text.lines()
                .enumerate()
                .filter(|(_, l)| cites_the_wrong_section(l))
                .map(move |(n, _)| format!("{path}:{}", n + 1))
        })
        .collect();
    assert!(
        bad.is_empty(),
        "section 7.3 is cited for Telegram: {bad:#?}"
    );
    // The self-test: the wrong citation is seen, the right one and another network's are not.
    let wrong = format!("\u{a7}7.{}", 3);
    assert!(cites_the_wrong_section(&format!(
        "// The Telegram backend (architecture {wrong})"
    )));
    assert!(cites_the_wrong_section(&format!(
        "# The TDLib helper ({wrong})"
    )));
    assert!(!cites_the_wrong_section(
        "// The Telegram backend (architecture \u{a7}7.5)"
    ));
    assert!(!cites_the_wrong_section(&format!(
        "// roaming changes like a rejected Threema transaction ({wrong})"
    )));
}

// ----- The receive path keeps and prints no raw JSON, and logs no value (TG-3 C11, TF12) -----

/// The modules that hold or handle TDLib's text (names, titles, message text, usernames, numbers):
/// receive, sync, store, content, markup, the lane, the session, the actor, the outbox and the
/// TDLib object types. Their tests are left out (TF12 widened the scope from receive/sync/store).
fn receive_files() -> Vec<(String, String)> {
    let scope = [
        "src/receive.rs",
        "src/receive/",
        "src/sync.rs",
        "src/sync/",
        "src/store.rs",
        "src/store/",
        "src/content.rs",
        "src/content/",
        "src/markup.rs",
        "src/markup/",
        "src/session.rs",
        "src/session/",
        "src/actor.rs",
        "src/outbox.rs",
        "src/tdapi.rs",
        "src/tdapi/",
    ];
    let files: Vec<(String, String)> = rust_files("crates/kourier-telegram/src")
        .into_iter()
        .filter(|(path, _)| {
            let in_scope = scope
                .iter()
                .any(|s| path.contains(&format!("kourier-telegram/{s}")));
            let test = path.ends_with("/tests.rs") || path.contains("/tests/");
            in_scope && !test
        })
        .collect();
    assert!(files.len() >= 16, "the receive files were not found");
    files
}

/// The end (exclusive) of the string literal that starts at `at` in `text` (an ordinary one with
/// escapes, or a raw one `r#"..."#`).
fn string_end(text: &str, at: usize) -> usize {
    let bytes = text.as_bytes();
    if bytes[at] == b'r' {
        let mut hashes = 0;
        let mut i = at + 1;
        while bytes.get(i) == Some(&b'#') {
            hashes += 1;
            i += 1;
        }
        let close = format!("\"{}", "#".repeat(hashes));
        return text[i + 1..]
            .find(&close)
            .map_or(text.len(), |e| i + 1 + e + close.len());
    }
    let mut i = at + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'"' => return i + 1,
            _ => i += 1,
        }
    }
    text.len()
}

/// Whether a string literal (ordinary or raw) starts at `at`.
fn string_starts(text: &str, at: usize) -> bool {
    let bytes = text.as_bytes();
    let before = text[..at].chars().next_back();
    match bytes[at] {
        // The char literal of a double quote is no string.
        b'"' => !(before == Some('\'') && text[at + 1..].starts_with('\'')),
        b'r' => {
            if before.is_some_and(|c| c.is_alphanumeric() || c == '_') {
                return false;
            }
            let mut i = at + 1;
            while bytes.get(i) == Some(&b'#') {
                i += 1;
            }
            bytes.get(i) == Some(&b'"')
        }
        _ => false,
    }
}

/// A macro call: its name (the last path segment) and the text between its parentheses.
struct MacroCall {
    name: String,
    args: String,
}

/// Every macro call `name!(..)` of `code` (string literals and nested brackets respected; the
/// char literal of a double quote too).
fn macro_calls(code: &str) -> Vec<MacroCall> {
    let mut calls = Vec::new();
    let bytes = code.as_bytes();
    let mut at = 0;
    while at + 1 < code.len() {
        if !code.is_char_boundary(at) {
            at += 1;
            continue;
        }
        if string_starts(code, at) {
            at = string_end(code, at);
            continue;
        }
        if bytes[at] == b'!' && bytes[at + 1] == b'(' {
            let name: String = code[..at]
                .chars()
                .rev()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            let start = at + 2;
            let mut depth = 1;
            let mut i = start;
            while i < code.len() && depth > 0 {
                if !code.is_char_boundary(i) {
                    i += 1;
                    continue;
                }
                if string_starts(code, i) {
                    i = string_end(code, i);
                    continue;
                }
                if code[i..].starts_with("'\"'") {
                    i += 3;
                    continue;
                }
                match bytes[i] {
                    b'(' | b'[' | b'{' => depth += 1,
                    b')' | b']' | b'}' => depth -= 1,
                    _ => {}
                }
                i += 1;
            }
            let end = i.saturating_sub(1).max(start);
            if !name.is_empty() {
                calls.push(MacroCall {
                    name,
                    args: code[start..end].to_owned(),
                });
            }
            at = start;
            continue;
        }
        at += 1;
    }
    calls
}

/// The arguments of a macro call: its top-level comma-separated parts, trimmed.
fn macro_args(args: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut depth = 0_i32;
    let mut from = 0;
    let mut at = 0;
    while at < args.len() {
        if !args.is_char_boundary(at) {
            at += 1;
            continue;
        }
        if string_starts(args, at) {
            at = string_end(args, at);
            continue;
        }
        match args.as_bytes()[at] {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            b',' if depth == 0 => {
                parts.push(args[from..at].trim().to_owned());
                from = at + 1;
            }
            _ => {}
        }
        at += 1;
    }
    let last = args[from..].trim();
    if !last.is_empty() {
        parts.push(last.to_owned());
    }
    parts
}

/// The placeholders of a format string: `(inline name, positional)`. A name is the text before a
/// `:` format spec; an empty or numeric one is positional. `{{` and `}}` are no placeholders.
fn placeholders(literal: &str) -> Vec<(String, bool)> {
    let mut out = Vec::new();
    let chars: Vec<char> = literal.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        match (chars[i], chars.get(i + 1)) {
            ('{', Some('{')) | ('}', Some('}')) => i += 2,
            ('{', _) => {
                let end = chars[i..]
                    .iter()
                    .position(|c| *c == '}')
                    .map_or(chars.len(), |p| i + p);
                let inner: String = chars[i + 1..end].iter().collect();
                let name = inner.split(':').next().unwrap_or("").trim().to_owned();
                let positional = name.is_empty() || name.chars().all(|c| c.is_ascii_digit());
                out.push((name, positional));
                i = end + 1;
            }
            _ => i += 1,
        }
    }
    out
}

/// A log field, a placeholder name or a printed value may be one of these ids, counts, kinds and
/// error names, and nothing else (TF12): a title, a name, a text or a number has none of them.
const LOG_NAMES: &[&str] = &[
    "id",
    "chat_id",
    "msg_id",
    "user_id",
    "count",
    "bytes",
    "wait_ms",
    "lockdown",
    "class",
    "act",
    "error",
    "code",
    "op",
    "kind",
    "n",
    "len",
    "e",
    "seq",
    "dropped",
    "reason",
    "refusal",
    // The login-start line's token: `on` or `off`, never the tail (KT-1b).
    "expected_number_check",
];

/// The expressions after a `?` or `%` sigil a log line may format: a store or link error (its
/// text names no value), an `Act` (its `Debug` is hand-written: kinds and ids), the outbox's
/// log-safe quarantine reason and a login `Refusal` (a `Copy` enum of fixed kinds).
const LOG_SIGILS: &[&str] = &["e", "other", "reason", "refusal"];

/// `format!` / `panic!` calls that build a VALUE (an id, a patch field), never a log line or an
/// error: each text is the call's argument text with its white space removed. A new one needs a
/// reason here.
const VALUE_FORMATS: &[&str] = &[
    // The generic line's kind name (a constructor name TDLib's schema fixed).
    "\"tg:{name}\"",
    // A shared contact's phone number: the digits go into the message's contact card only.
    "\"+{digits}\"",
    // A shared contact's name: the card's display name only.
    "\"{}{}\",c.first_name.as_str().trim(),c.last_name.as_str().trim()",
    // A user's name and number: the contact patch only.
    "\"{}{}\",user.first_name.as_str(),user.last_name.as_str()",
    "\"+{}\",user.phone_number.as_str()",
    // A conversation or contact id from TDLib's numeric ids (`tg:<id>`), never a name.
    "\"tg:{me}\"",
    "\"tg:{}\",chat.id",
];

/// What the log walls refuse in one macro call of the receive scope (empty: fine).
fn macro_violations(call: &MacroCall) -> Vec<String> {
    let squeezed = squeezed(&call.args);
    let name = call.name.as_str();
    let mut bad = Vec::new();
    // Never in non-test code: they print what they are given.
    if ["dbg", "println", "eprintln", "print", "eprint"].contains(&name) {
        bad.push(format!("{name}! in non-test code"));
        return bad;
    }
    let tracing = ["trace", "debug", "info", "warn", "error"].contains(&name);
    let formats = ["format", "panic", "unreachable", "todo", "unimplemented"].contains(&name);
    if !tracing && !formats {
        return bad;
    }
    if formats && VALUE_FORMATS.contains(&squeezed.as_str()) {
        return bad;
    }
    let args = macro_args(&call.args);
    let is_literal = |a: &str| string_starts(a, 0);
    // The message (the format string) and the values it is given.
    let Some(message) = args.iter().position(|a| is_literal(a)) else {
        // `panic!()` and the like: nothing printed.
        if tracing {
            bad.push(format!("{name}! without a message"));
        }
        return bad;
    };
    let holes = placeholders(&args[message]);
    for (hole, positional) in &holes {
        if !positional && !LOG_NAMES.contains(&hole.as_str()) {
            bad.push(format!("{name}! prints the inline name {hole}"));
        }
    }
    if holes.iter().any(|(_, positional)| *positional) {
        // Positional holes take the values after the message: each must be a listed name.
        for value in &args[message + 1..] {
            let value = value.split('=').next_back().unwrap_or("").trim();
            if !LOG_NAMES.contains(&value) {
                bad.push(format!("{name}! prints a value `{value}`"));
            }
        }
    }
    if tracing {
        // The fields before the message: `name`, `name = expr`, `?name`, `%name`, `name = ?expr`.
        for field in &args[..message] {
            let (key, value) = field
                .split_once('=')
                .map_or((field.as_str(), ""), |(k, v)| (k, v));
            let key = key.trim();
            let value = value.trim();
            let sigil_on_key = key.starts_with(['?', '%']);
            let key = key.trim_start_matches(['?', '%']).trim();
            if !LOG_NAMES.contains(&key) {
                bad.push(format!("{name}! has the field `{key}`"));
            }
            let sigil_value = value.starts_with(['?', '%']);
            let expr = value.trim_start_matches(['?', '%']).trim();
            if (sigil_on_key || sigil_value)
                && !LOG_SIGILS.contains(&(if sigil_on_key { key } else { expr }))
            {
                bad.push(format!("{name}! formats a field with ? or %"));
            }
        }
    }
    bad
}

/// What a receive file does that the log rule (architecture §9.1) or the proxy rule forbids.
fn receive_violations(text: &str) -> Vec<String> {
    let code = code(text);
    let mut bad = Vec::new();
    let added = ["added", "Proxy"].concat();
    if code.contains(&added) {
        bad.push(format!("keeps an {added}"));
    }
    // `{:?}`, `{x:?}`, `{:#?}` (inside a format string: the literal is looked at too), except in
    // `write!(f, ..)` of a hand-written `Debug` impl (its kinds and ids are reviewed: `Act`, the
    // login steps).
    let mut checked = code.clone();
    for call in macro_calls(&code) {
        if ["write", "writeln"].contains(&call.name.as_str())
            && call.args.trim_start().starts_with("f,")
        {
            checked = checked.replace(&call.args, "");
        }
    }
    for needle in [
        [":", "?}"].concat(),
        [":#", "?}"].concat(),
        ["to_string", "_pretty"].concat(),
    ] {
        if checked.contains(&needle) {
            bad.push(format!("formats with {needle}"));
        }
    }
    // A JSON text is made only from a `json!` literal the code wrote (a parked row's payload).
    let to_string = ["serde_json", "::to_string(&"].concat();
    for (at, _) in code.match_indices(&to_string) {
        let after = code[at + to_string.len()..].trim_start();
        if !(after.starts_with("serde_json::json!(") || after.starts_with("json!(")) {
            bad.push("serializes a value it did not write".to_owned());
        }
    }
    // The log walls (TF12): a log line, a `format!` and a `panic!` print ids, counts, kinds and
    // errors only; the print macros are not used at all.
    for call in macro_calls(&code) {
        bad.extend(macro_violations(&call));
    }
    bad
}

/// TG-3 C11 + TF12: no receive, sync, store, content, markup, lane, session, actor, outbox or TDLib
/// type code formats a TDLib value (`Debug` / `Display` of a JSON value or of a field into a log
/// line or a string) or keeps TDLib's proxy answer; a log line, `format!` or `panic!` has no
/// positional argument and no inline name outside [`LOG_NAMES`], and no field with `?` / `%` but an
/// error or an `Act`. What it cannot see: a `Value` turned into text by a call this list does not
/// name (`.to_string()` on a value whose type the text does not show), a `write!` into a `Debug`
/// impl; the log capture of the receive run is the wall behind it.
#[test]
fn the_receive_path_formats_no_tdlib_value() {
    let bad: Vec<String> = receive_files()
        .iter()
        .flat_map(|(path, text)| {
            receive_violations(text)
                .into_iter()
                .map(move |v| format!("{path}: {v}"))
        })
        .collect();
    assert!(bad.is_empty(), "the receive path: {bad:#?}");
}

/// Each receive check catches its sample, and the allowed forms pass.
#[test]
fn every_receive_check_catches_its_sample() {
    let level = ["tracing", "::debug!("].concat();
    let bare = ["debug", "!("].concat();
    let samples = [
        format!("let a = {};", ["\"added", "Proxy\""].concat()),
        format!("{level}chat = ?chat, \"a chat\");"),
        format!("{level}title = %chat.title);"),
        format!("{level}\"a user {{}}\", user.first_name.as_str());"),
        format!("{level}\"{{}}\", c.first_name.as_str());"),
        format!("{level}\"a user {{name}}\");"),
        format!("{level}chat_id, \"a user {{}}\", user.title.as_str());"),
        format!("{level}title = chat.title.as_str(), \"a chat\");"),
        format!("{level}?paging, \"a page\");"),
        format!("{bare}\"a user {{}}\", user.first_name.as_str());"),
        "let t = format!(\"{}\", chat.title.as_str());".to_owned(),
        "let t = format!(\"{name}\");".to_owned(),
        format!("{}(\"{{}}\", user.first_name);", ["pan", "ic!"].concat()),
        format!("{}(\"hello\");", ["print", "ln!"].concat()),
        format!("{}(\"hello\");", ["eprint", "ln!"].concat()),
        format!("{}(chat);", ["db", "g!"].concat()),
        format!("let t = format!(\"{}\", raw);", ["{:", "?}"].concat()),
        format!("let t = format!(\"{{raw{}\");", [":", "?}"].concat()),
        format!(
            "let t = serde_json::{}(&raw);",
            ["to_string", "_pretty"].concat()
        ),
        "let t = serde_json::to_string(&raw);".to_owned(),
    ];
    for sample in &samples {
        assert!(
            !receive_violations(sample).is_empty(),
            "a sample passed: {sample}"
        );
    }
    for fine in [
        format!("{level}chat_id, \"TDLib refused a page?\");"),
        format!("{level}chat_id, \"chat {{chat_id}} has {{}} rows\", count);"),
        format!("{level}\"a read marker waits for the helper\");"),
        format!("{level}act = ?other, \"an act is dropped\");"),
        format!("{level}bytes, \"the files directory is emptied\");"),
        format!("{level}wait_ms = duration_ms(wait), \"again after the back-off\");"),
        ["tracing", "::error!(error = %e, \"failed\");"].concat(),
        "let id = format!(\"tg:{id}\");".to_owned(),
        "let t = format!(\"tg:{name}\");".to_owned(),
        "let p = serde_json::to_string(&serde_json::json!({ \"ids\": ids }));".to_owned(),
        "// keeps an addedProxy in prose".to_owned(),
        "let q = \"a string with print!( inside\";".to_owned(),
    ] {
        assert!(
            receive_violations(&fine).is_empty(),
            "an allowed form failed: {fine} -> {:?}",
            receive_violations(&fine)
        );
    }
}

// ----- KT-1b: the words the fake that plays Telegram and this backend share -----

/// The message and the field names of the one INFO call in `path` (relative to the root) that says
/// `message`: what a run tool reads, without a log capture (the fake's crate has none).
fn info_line(path: &str, message: &str) -> (String, Vec<String>) {
    let text = std::fs::read_to_string(root().join(path)).expect("a source file");
    let calls: Vec<MacroCall> = macro_calls(&code(&text))
        .into_iter()
        .filter(|c| c.name == "info" && c.args.contains(message))
        .collect();
    assert_eq!(
        calls.len(),
        1,
        "{path}: exactly one INFO call says {message:?}"
    );
    let args = macro_args(&calls[0].args);
    let (said, fields) = args.split_last().expect("a message");
    let fields = fields
        .iter()
        .map(|f| f.split('=').next().unwrap_or("").trim().to_owned())
        .collect();
    (said.trim_matches('"').to_owned(), fields)
}

/// The login-start line (KT-1b F4, D-2): the fake that plays Telegram (`kourier-fake`'s
/// `actor/phone.rs`) and this backend (`actor.rs`) print the same message with the same field
/// names, and both give the field the two values `on` and `off`, so the run tool's token ("type the
/// number now") reads either. The fake's crate has no log capture, so its words are pinned by its
/// source; this backend's by the captured test `actor::tests::a_login_start_says_whether_the_number_is_checked_and_never_the_tail`.
/// Mutant: a word of either side.
#[test]
fn the_fake_and_the_backend_print_the_same_login_start_line() {
    let message = "a telegram login starts";
    let fake = info_line("crates/kourier-fake/src/actor/phone.rs", message);
    let real = info_line("crates/kourier-telegram/src/actor.rs", message);
    assert_eq!(fake, real, "the fake and the backend say different things");
    assert_eq!(real.0, message);
    assert_eq!(
        real.1,
        ["expected_number_check"],
        "one field, never the tail"
    );
    for path in [
        "crates/kourier-fake/src/actor/phone.rs",
        "crates/kourier-telegram/src/login.rs",
    ] {
        let text = code(&std::fs::read_to_string(root().join(path)).expect("a source file"));
        assert!(
            text.contains("{ \"on\" } else { \"off\" }"),
            "{path} gives the token neither `on` nor `off`"
        );
    }
}

/// `resend` is defined once, in the model (`kourier_model::RESEND_FIELD`): this backend reads it and the
/// fake that plays Telegram writes no copy of the word. Mutant: the backend's alias made a literal.
#[test]
fn the_resend_field_has_one_definition() {
    let model = std::fs::read_to_string(root().join("crates/kourier-model/src/login.rs"))
        .expect("the model's login.rs");
    assert!(code(&model).contains("pub const RESEND_FIELD: &str = \"resend\";"));
    let login = code(
        &std::fs::read_to_string(root().join("crates/kourier-telegram/src/login.rs"))
            .expect("the backend's login.rs"),
    );
    assert!(
        login.contains("pub const RESEND: &str = kourier_model::RESEND_FIELD;"),
        "the backend's field id is the model's constant"
    );
    for dir in ["crates/kourier-telegram/src", "crates/kourier-fake/src"] {
        for (path, text) in rust_files(dir) {
            let test = path.ends_with("/tests.rs") || path.contains("/tests/");
            if !test {
                assert!(
                    !code(&text).contains("\"resend\""),
                    "{path} writes the word `resend` itself"
                );
            }
        }
    }
}

// ----- TR R0a D-12: one TCP connect, one place that chooses to dial -----

/// The text as single spaces with none around `::`, so a spelling split over lines or written with
/// spaces is still one needle.
fn flat(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace(" ::", "::")
        .replace(":: ", "::")
}

/// The characters a `'x'` literal may hold that would count as a bracket.
const BRACKET_CHARS: &[u8] = b"{}()[]";

/// How many bytes the string, character literal or comment starting at `i` takes, if one does.
/// Strings, `'{'`-like characters, escaped character literals (`'\''`, `'\"'`, `'\\'`, `'\n'`,
/// `'\u{7d}'`: the escape's quote or brace is no string start or bracket), `//` comments and
/// `/* .. */` comments (nested, as Rust's are) do not count as brackets or separators (re-check N-4).
fn skip_literal(text: &str, i: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    if matches!(bytes[i], b'"' | b'r') && string_starts(text, i) {
        return Some(string_end(text, i) - i);
    }
    if bytes[i] == b'\'' {
        if bytes.get(i + 2) == Some(&b'\'')
            && bytes.get(i + 1).is_some_and(|c| BRACKET_CHARS.contains(c))
        {
            return Some(3);
        }
        // `'\X'`: the escaped character is never the closing quote; a lifetime has no backslash.
        if bytes.get(i + 1) == Some(&b'\\')
            && let Some(close) = bytes
                .get(i + 3..)
                .and_then(|rest| rest.iter().position(|&b| b == b'\''))
        {
            return Some(close + 4);
        }
    }
    if bytes[i..].starts_with(b"/*") {
        let (mut depth, mut j) = (0usize, i);
        while j < bytes.len() {
            if bytes[j..].starts_with(b"/*") {
                depth += 1;
                j += 2;
            } else if bytes[j..].starts_with(b"*/") {
                depth -= 1;
                j += 2;
                if depth == 0 {
                    return Some(j - i);
                }
            } else {
                j += 1;
            }
        }
        return Some(bytes.len() - i);
    }
    if bytes[i..].starts_with(b"//") {
        return Some(
            bytes[i..]
                .iter()
                .position(|&b| b == b'\n')
                .unwrap_or(bytes.len() - i),
        );
    }
    None
}

/// The end (exclusive) of the block item at the start of `text`: its first `{ .. }` block.
fn item_end(text: &str) -> usize {
    let (mut depth, mut i) = (0usize, 0usize);
    while i < text.len() {
        if let Some(skip) = skip_literal(text, i) {
            i += skip;
            continue;
        }
        match text.as_bytes()[i] {
            b'{' => depth += 1,
            b'}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return i + 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    text.len()
}

/// Whether the text after a `#[cfg(test)]` starts an ITEM (`mod`, `fn`, `use`, `impl`, ... after
/// any further attributes and a `pub`): such an item ends at its `;` or its first block and never
/// at a comma (`impl<A, B> ..`, `fn f<A, B>()`); a match arm, a field or a statement ends at its
/// first `;` or `,` as well.
fn starts_an_item(text: &str) -> bool {
    let ident = |c: char| c.is_alphanumeric() || c == '_';
    let mut rest = text.trim_start();
    while rest.starts_with("#[") {
        let mut depth = 0usize;
        let mut end = rest.len();
        for (i, c) in rest.char_indices() {
            match c {
                '[' => depth += 1,
                ']' => {
                    depth -= 1;
                    if depth == 0 {
                        end = i + 1;
                        break;
                    }
                }
                _ => {}
            }
        }
        rest = rest[end..].trim_start();
    }
    if let Some(after) = rest.strip_prefix("pub").filter(|a| !a.starts_with(ident)) {
        rest = after.trim_start();
        if rest.starts_with('(') {
            rest = rest.split_once(')').map_or("", |(_, r)| r.trim_start());
        }
    }
    let word: String = rest.chars().take_while(|c| ident(*c)).collect();
    matches!(
        word.as_str(),
        "fn" | "mod"
            | "use"
            | "impl"
            | "struct"
            | "enum"
            | "trait"
            | "type"
            | "static"
            | "const"
            | "union"
            | "unsafe"
            | "async"
            | "extern"
            | "macro_rules"
    )
}

/// Whether `text` goes on with an `else` (an `if` statement's other branch).
fn else_follows(text: &str) -> bool {
    text.trim_start()
        .strip_prefix("else")
        .is_some_and(|rest| !rest.starts_with(|c: char| c.is_alphanumeric() || c == '_'))
}

/// The end (exclusive) of the item that a `#[cfg(test)]` gates, `text` starting right after the
/// attribute: its first `;` or `,` at depth 0 (a `,` ends no ITEM, see [`starts_an_item`]) or the end
/// of its own first block (with its `else` branches). `None` when the close of the ENCLOSING block
/// (or of a bracket this item never opened) comes first, or the text ends: the item cannot be told
/// from the code behind it, and the caller fails loudly instead of stripping what it cannot see.
fn gated_end(text: &str) -> Option<usize> {
    let item = starts_an_item(text);
    let (mut depth, mut i) = (0usize, 0usize);
    while i < text.len() {
        if let Some(skip) = skip_literal(text, i) {
            i += skip;
            continue;
        }
        match text.as_bytes()[i] {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' => depth = depth.checked_sub(1)?,
            b'}' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 && !else_follows(&text[i + 1..]) {
                    return Some(i + 1);
                }
            }
            b';' if depth == 0 => return Some(i + 1),
            b',' if depth == 0 && !item => return Some(i + 1),
            _ => {}
        }
        i += 1;
    }
    None
}

/// The text with its comment lines emptied (the line numbers stay those of the file).
fn blank_comment_lines(text: &str) -> String {
    text.lines()
        .map(|l| {
            if l.trim_start().starts_with("//") {
                ""
            } else {
                l
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The code of a file without its comment lines and without every item gated by `#[cfg(test)]`
/// (an inline test module, a gated statement, a match arm, a field, a gated `mod tests;`). An item
/// that would run past the close of its enclosing block is an `Err` with the LINE of its attribute:
/// the strip never drops what it cannot delimit (TR R0a fix G-2). What it cannot see: a gate written
/// another way (`cfg(all(test, ..))`) and the test-only files a `#[cfg(test)] mod x;` line names
/// (they are scanned as code).
fn non_test_code(text: &str) -> Result<String, usize> {
    const GATE: &str = "#[cfg(test)]";
    let plain = blank_comment_lines(text);
    let mut out = String::new();
    let (mut rest, mut consumed) = (plain.as_str(), 0usize);
    while let Some(at) = rest.find(GATE) {
        out.push_str(&rest[..at]);
        let after = &rest[at + GATE.len()..];
        let Some(len) = gated_end(after) else {
            return Err(plain[..consumed + at].matches('\n').count() + 1);
        };
        consumed += at + GATE.len() + len;
        rest = &after[len..];
    }
    out.push_str(rest);
    Ok(out)
}

/// What a file tells when its `#[cfg(test)]` strip fails: the file and the line.
fn strip_failure(path: &str, line: usize) -> String {
    format!(
        "{path}:{line}: a #[cfg(test)] item runs past the close of its block: gate a whole item"
    )
}

/// Whether `path` is a test file (a `tests.rs`, or below a `tests/` directory).
fn is_test_path(path: &str) -> bool {
    path.ends_with("/tests.rs") || path.contains("/tests/")
}

/// The one file whose non-test code opens a TCP connection: the dialer of the Telegram mode.
const CONNECT_FILES: &[&str] = &["crates/kourier-telegram/src/real.rs"];

/// The spellings that open (or prepare) a network connection, assembled at run time.
fn connect_spellings() -> Vec<String> {
    let parts: &[&[&str]] = &[
        &["Tcp", "Stream::connect"],
        &["connect", "_timeout"],
        &["Tcp", "Socket::new_v4"],
        &["Tcp", "Socket::new_v6"],
        &["socket", "2::"],
        &["li", "bc::connect"],
        &["Tcp", "Stream as"],
        &["Tcp", "Socket as"],
        &["Tcp", "Stream::from_std"],
        &["Udp", "Socket"],
        &["lookup", "_host"],
        &["to_socket", "_addrs"],
        &["rus", "tix::net"],
        &["ni", "x::sys::socket"],
        &["li", "bc::syscall"],
    ];
    parts.iter().map(|p| p.concat()).collect()
}

/// Whether a `type X = ..;` item of the flat text names `name`: an alias hides a spelling behind
/// its own name (`type Plain = ..TcpStream; Plain::connect(..)`; TR R0a fix G-2).
fn type_alias_names(flat: &str, name: &str) -> bool {
    name_positions(flat, "type").into_iter().any(|at| {
        let item = &flat[at..];
        let item = &item[..item.find(';').unwrap_or(item.len())];
        item.contains('=') && names(item, name)
    })
}

/// The glob imports of the paths that hold the connect spellings: `use libc::*`, `use
/// tokio::net::*`, `use std::net::*`, `use socket2::*` (nested too: `use tokio::{net::*}`) and a
/// `*` in a list of such a path (`use libc::{self, *}`). Neither scanned tree needs a glob
/// (fix G-2).
fn connect_globs(flat: &str) -> Vec<String> {
    let ident = |c: char| c.is_alphanumeric() || c == '_';
    let holders = [
        ["li", "bc"].concat(),
        "net".to_owned(),
        ["socket", "2"].concat(),
    ];
    let mut found = Vec::new();
    for (at, _) in flat.match_indices("::*") {
        let segment: String = flat[..at].chars().rev().take_while(|c| ident(*c)).collect();
        let segment: String = segment.chars().rev().collect();
        if holders.contains(&segment) {
            found.push(format!("{segment}::*"));
        }
    }
    for holder in &holders {
        for (at, _) in flat.match_indices(&format!("{holder}::{{")) {
            let list = &flat[at..];
            let list = &list[..list.find('}').unwrap_or(list.len())];
            if list.split(',').any(|item| item.trim() == "*") {
                found.push(format!("{holder}::{{*}}"));
            }
        }
    }
    found
}

/// What opens a connection in the non-test code of `kourier-telegram/src` (outside the dialer's
/// file) and of `kourier-app/src` (nowhere): the spellings above, also named from a `libc::{..}`
/// list, behind a type alias of a socket type, or behind a glob import of the paths that hold them.
fn tcp_connect_violations(path: &str, text: &str) -> Vec<String> {
    let scanned = path.starts_with("crates/kourier-telegram/src/")
        || path.starts_with("crates/kourier-app/src/");
    if !scanned || is_test_path(path) || CONNECT_FILES.contains(&path) {
        return Vec::new();
    }
    let plain = match non_test_code(text) {
        Ok(plain) => plain,
        Err(line) => return vec![strip_failure(path, line)],
    };
    let flat = flat(&plain);
    let mut bad: Vec<String> = connect_spellings()
        .into_iter()
        .filter(|spelling| names(&flat, spelling))
        .map(|spelling| format!("{path} names {spelling}"))
        .collect();
    let listed = use_list_names(&plain, &["li", "bc::{"].concat());
    for name in ["connect", "syscall"] {
        if listed.iter().any(|n| n == name) {
            bad.push(format!("{path} imports libc {name}"));
        }
    }
    for socket in [["Tcp", "Stream"], ["Tcp", "Socket"], ["Udp", "Socket"]] {
        let socket = socket.concat();
        if type_alias_names(&flat, &socket) {
            bad.push(format!("{path} aliases {socket}"));
        }
    }
    bad.extend(
        connect_globs(&flat)
            .into_iter()
            .map(|glob| format!("{path} imports {glob}")),
    );
    bad
}

/// TR R0a D-12 (a): the TCP dialer of the Telegram mode is the only non-test code of this crate
/// that opens a connection, and `kourier-app/src` opens none at all (the application reaches the
/// network only through the parts it hands the backend). What it cannot see: a connect made through
/// a crate this list does not name, or a name assembled at run time.
/// Mutant: one spelling dropped from the list (`every_connect_spelling_is_seen` fails).
#[test]
fn only_real_rs_opens_a_tcp_connection() {
    let mut bad = Vec::new();
    let mut real = None;
    for (path, text) in lane_files() {
        bad.extend(tcp_connect_violations(&path, &text));
        if CONNECT_FILES.contains(&path.as_str()) {
            real = Some(text);
        }
    }
    assert!(bad.is_empty(), "{bad:#?}");
    // The check is not vacuous: the dialer's file does connect, and the strip of its test module
    // leaves exactly that one (the test module connects too).
    let real = real.expect("the dialer's file is scanned");
    let connect = &connect_spellings()[0];
    assert_eq!(
        flat(&non_test_code(&real).expect("the dialer's file strips"))
            .matches(connect.as_str())
            .count(),
        1,
        "the dialer's own connect is not seen in its non-test code"
    );
    assert!(
        flat(&code(&real)).matches(connect.as_str()).count() > 1,
        "the test module of the dialer's file is not in the text the strip works on"
    );
}

/// One sample per spelling, assembled at run time, seen in both scanned places and not where it
/// may be (the dialer's file, a test file, a test module, a comment).
#[test]
fn every_connect_spelling_is_seen() {
    let samples: Vec<String> = [
        "let s = tokio::net::TcpStream::connect(to).await;",
        "let s = std::net::TcpStream::connect_timeout(&to, t);",
        "let s = tokio::net::TcpSocket::new_v4();",
        "let s = tokio::net::TcpSocket::new_v6();",
        "let s = socket2::Socket::new(d, t, p);",
        "let r = unsafe { libc::connect(fd, a, l) };",
        "use tokio::net::TcpStream as Plain;",
        "use tokio::net::TcpSocket as Plain;",
        "let s = tokio::net::TcpStream::from_std(raw);",
        "let s = tokio::net::UdpSocket::bind(a);",
        "let a = tokio::net::lookup_host(h).await;",
        "let a = (h, 443).to_socket_addrs();",
        "let s = rustix::net::socket(d, t, p);",
        "let s = nix::sys::socket::socket(d, t, f, p);",
        "let r = unsafe { libc::syscall(n) };",
    ]
    .iter()
    .map(|sample| (*sample).to_owned())
    .collect();
    let spellings = connect_spellings();
    assert_eq!(
        samples.len(),
        spellings.len(),
        "a spelling without its own sample"
    );
    for (n, sample) in samples.iter().enumerate() {
        for path in [
            "crates/kourier-telegram/src/actor.rs",
            "crates/kourier-app/src/app.rs",
        ] {
            let seen = tcp_connect_violations(path, sample);
            assert!(
                seen.iter().any(|v| v.ends_with(&spellings[n])),
                "spelling {n} ({}) is not seen in {path}",
                spellings[n]
            );
        }
        // The dialer's file, a test file and a comment may say it; so may a test module.
        let dialer = CONNECT_FILES[0];
        assert!(tcp_connect_violations(dialer, sample).is_empty());
        assert!(
            tcp_connect_violations("crates/kourier-telegram/src/gate/tests.rs", sample).is_empty()
        );
        assert!(
            tcp_connect_violations("crates/kourier-app/src/app.rs", &format!("// {sample}"))
                .is_empty()
        );
        let in_tests = format!(
            "fn real() {{ let b = '{{'; }}\n#[cfg(test)]\nmod tests {{\n    fn t() {{ let c = '}}'; {sample} }}\n}}\nfn after() {{ f(\"}}\"); }}"
        );
        assert!(
            tcp_connect_violations("crates/kourier-app/src/app.rs", &in_tests).is_empty(),
            "a test module's connect is taken for code: {sample}"
        );
        // Code after the test module is still code.
        let after = format!("{in_tests}\nfn later() {{ {sample} }}");
        assert!(
            !tcp_connect_violations("crates/kourier-app/src/app.rs", &after).is_empty(),
            "code after a test module escaped the check: {sample}"
        );
    }
    // The imports of a libc list, and what is not a connect.
    let listed = ["use li", "bc::{c_int, connect};"].concat();
    assert!(!tcp_connect_violations("crates/kourier-app/src/app.rs", &listed).is_empty());
    // Fix G-2 (review F2): a type alias hides the spelling (`Plain::connect(..)` names none of
    // them), and a glob import hides the path (`connect(fd, ..)` names none either).
    let alias = |socket: &[&str]| {
        format!(
            "type Plain = tokio::net::{};\nfn f(to: A) {{ Plain::connect(to); }}",
            socket.concat()
        )
    };
    let glob =
        |path: &[&str]| format!("use {}::*;\nfn f() {{ connect(fd, a, l); }}", path.concat());
    let samples = [
        ("an alias of a stream", alias(&["Tcp", "Stream"]), "aliases"),
        ("an alias of a socket", alias(&["Tcp", "Socket"]), "aliases"),
        (
            "an alias of a datagram socket",
            alias(&["Udp", "Socket"]),
            "aliases",
        ),
        (
            "a pub alias in a block",
            format!(
                "mod m {{ pub type Plain = {}; }}",
                ["std::net::Tcp", "Stream"].concat()
            ),
            "aliases",
        ),
        ("a libc glob", glob(&["li", "bc"]), "imports"),
        ("a tokio net glob", glob(&["tokio::", "net"]), "imports"),
        ("a std net glob", glob(&["std::", "net"]), "imports"),
        ("a socket2 glob", glob(&["socket", "2"]), "imports"),
        (
            "a nested net glob",
            ["use tokio::{net::", "*, time};"].concat(),
            "imports",
        ),
        (
            "a star in a libc list",
            ["use li", "bc::{self, *};"].concat(),
            "imports",
        ),
    ];
    for (what, sample, word) in &samples {
        for path in [
            "crates/kourier-telegram/src/actor.rs",
            "crates/kourier-app/src/app.rs",
        ] {
            let seen = tcp_connect_violations(path, sample);
            assert!(
                seen.iter().any(|v| v.contains(word)),
                "{what} is not seen in {path}: {seen:?}"
            );
            assert!(
                tcp_connect_violations(path, &format!("// {sample}")).is_empty(),
                "{what}: a comment is taken for code"
            );
        }
        assert!(
            tcp_connect_violations(CONNECT_FILES[0], sample).is_empty(),
            "{what}: the dialer's file may say it"
        );
        assert!(
            tcp_connect_violations("crates/kourier-telegram/src/gate/tests.rs", sample).is_empty(),
            "{what}: a test file may say it"
        );
        let in_tests = format!("#[cfg(test)]\nmod tests {{\n{sample}\n}}\nfn after() {{}}");
        assert!(
            tcp_connect_violations("crates/kourier-app/src/app.rs", &in_tests).is_empty(),
            "{what}: a test module is taken for code"
        );
    }
    for fine in [
        "use super::*;\ntype Shared = Arc<dyn Dialer>;\ntype Plain = MyTcpStream;",
        "use std::net::{IpAddr, SocketAddr};\ntype Addr = SocketAddr;\nlet product = a * b;",
        "impl Iterator for X { type Item = u8; }",
    ] {
        assert!(
            tcp_connect_violations("crates/kourier-app/src/app.rs", fine).is_empty(),
            "taken for a connect: {fine}"
        );
    }
    for fine in [
        "let a = peer.connect_to_gate(); let b = reconnect_timeouts();",
        "let n = other.connected_count(); let x = MyTcpStream::new();",
        "use std::net::{IpAddr, SocketAddr};",
    ] {
        assert!(
            tcp_connect_violations("crates/kourier-app/src/app.rs", fine).is_empty(),
            "taken for a connect: {fine}"
        );
    }
}

/// Fix G-2 (review F2): the strip of `#[cfg(test)]` items ends an item where the item ends. A gated
/// match arm, field or statement no longer hides the code behind it in the same block; an item the
/// strip cannot delimit (it would run past the close of its block) fails loudly with its line, it
/// never strips silently. Whole items (a module, a function with a comma in its generics, an `if`
/// with its `else`) are still stripped whole.
#[test]
fn the_test_strip_ends_an_item_where_the_item_ends() {
    let connect = ["tokio::net::Tcp", "Stream::connect(to)"].concat();
    let app = "crates/kourier-app/src/app.rs";
    let seen = |text: &str| !tcp_connect_violations(app, text).is_empty();
    // Code behind a gated item of the same block is code.
    for (what, text) in [
        (
            "a gated match arm",
            format!(
                "fn f(x: u8) {{ match x {{ #[cfg(test)] 1 => g(a, b), _ => {{ let _ = {connect}; }} }} }}"
            ),
        ),
        (
            "a gated match arm with a block",
            format!(
                "fn f(x: u8) {{ match x {{ #[cfg(test)] 1 => {{ g(a, b) }} _ => {{ let _ = {connect}; }} }} }}"
            ),
        ),
        (
            "a gated statement",
            format!("fn f() {{ #[cfg(test)] let t = g(a, b); let _ = {connect}; }}"),
        ),
        (
            "a gated field",
            format!("fn f() {{ let s = S {{ #[cfg(test)] t: 1, u: {{ {connect} }} }}; }}"),
        ),
        (
            "an if after a gated item",
            format!("#[cfg(test)]\nfn t() {{}}\nfn f() {{ if x {{ let _ = {connect}; }} }}"),
        ),
    ] {
        assert!(seen(&text), "{what} hid the code behind it: {text}");
    }
    // Whole items are stripped whole.
    for (what, text) in [
        (
            "a module",
            format!("#[cfg(test)]\nmod tests {{ fn t() {{ let _ = {connect}; }} }}"),
        ),
        (
            "a function with parameters and generics",
            format!("#[cfg(test)]\nfn t<A, B>(a: A, b: B) {{ let _ = {connect}; }}"),
        ),
        (
            "an impl with generics",
            format!("#[cfg(test)]\nimpl<A, B> X<A, B> {{ fn t(&self) {{ let _ = {connect}; }} }}"),
        ),
        (
            "an item with more attributes and a pub",
            format!(
                "#[cfg(test)]\n#[allow(dead_code)]\npub(crate) struct S {{ a: u8, b: u8 }}\n#[cfg(test)]\n#[derive(Debug, Clone)]\npub fn t() {{ let _ = {connect}; }}"
            ),
        ),
        (
            "an if statement with its else",
            format!(
                "fn f() {{ #[cfg(test)] if x {{ let _ = {connect}; }} else {{ let _ = {connect}; }} }}"
            ),
        ),
        (
            "a use list and a mod declaration",
            "#[cfg(test)]\nuse a::{b, c};\n#[cfg(test)]\nmod tests;".to_owned(),
        ),
        (
            "a const with brackets",
            "#[cfg(test)]\nconst X: [u8; 2] = [1, 2];\nfn f() {}".to_owned(),
        ),
    ] {
        assert!(
            tcp_connect_violations(app, &text).is_empty(),
            "{what} was not stripped whole: {text}"
        );
    }
    // Re-check N-4: a bracket inside a block comment and a quote inside an escaped character literal
    // are no bracket and no string start, so the code behind a gated item stays code (a text rule
    // that took them for brackets or a string would strip `real` with the item).
    for (what, text) in [
        (
            "a brace in a block comment of a gated item",
            format!(
                "#[cfg(test)]\nfn t() {{ /* {{ */ }}\nfn real() {{ {connect}; }}\nfn z() {{ /* }} */ }}"
            ),
        ),
        (
            "a brace in a nested block comment",
            // (a comment that does not nest ends at its first `*/`, the item then ends at the last
            // brace and `real` is stripped with it; a loud failure would count as seen too)
            format!("#[cfg(test)]\nfn t() {{ /* /* */ {{ */ }}\nfn real() {{ {connect}; }}\n}}"),
        ),
        (
            // (the escaped quote of the first char opens a string when it is not skipped; the plain
            // quote char of `z` closes it, so `real` is swallowed with no loud failure)
            "an escaped double quote",
            format!(
                "#[cfg(test)]\nfn t() {{ let c = '\\\"'; }}\nfn real() {{ {connect}; }}\nfn z() {{ let c = '\"'; }}"
            ),
        ),
        (
            "an escaped single quote, a backslash and a newline",
            format!(
                "#[cfg(test)]\nfn t<'a>(x: &'a str) -> [char; 3] {{ ['\\'', '\\\\', '\\n'] }}\nfn real() {{ {connect}; }}"
            ),
        ),
    ] {
        assert!(seen(&text), "{what} hid the code behind it: {text}");
    }
    // ... and an item whose block comment holds the closing brace is stripped whole.
    let commented = format!("#[cfg(test)]\nfn t() {{ /* }} */ let _ = {connect}; }}");
    assert!(
        tcp_connect_violations(app, &commented).is_empty(),
        "a brace in a block comment ended the gated item early: {commented}"
    );
    // An item that runs past the close of its block fails loudly, with the line of its attribute.
    for (what, text, line) in [
        (
            "a gated trailing expression",
            "fn f() {\n    #[cfg(test)]\n    g(x)\n}".to_owned(),
            2,
        ),
        (
            "a gated last argument",
            "fn f() {\n    h(a,\n      #[cfg(test)] b)\n}".to_owned(),
            3,
        ),
        (
            "a gated item that never ends",
            "fn f() {}\n\n#[cfg(test)] fn".to_owned(),
            3,
        ),
    ] {
        let failure = tcp_connect_violations(app, &text);
        assert_eq!(
            failure,
            vec![strip_failure(app, line)],
            "{what} was not refused with its line"
        );
    }
    // The comment lines keep the line count: a comment above does not move the line.
    let commented = "// one\n// two\nfn f() {\n    #[cfg(test)]\n    g(x)\n}";
    assert_eq!(
        tcp_connect_violations(app, commented),
        vec![strip_failure(app, 4)]
    );
}

/// The one file that may name the variant that dials: the parts' module.
const DIAL_MODE_FILE: &str = "crates/kourier-telegram/src/real.rs";
/// The one other file that may: the application's confirmation.
const DIAL_CHOOSER_FILE: &str = "crates/kourier-app/src/telegram.rs";
/// The function of that file that chooses (not its `_as` variants).
const DIAL_CHOOSER_FN: &str = "fn dial_confirmation";
/// The third file that may name the TYPE (never the variant): the application's composition, which
/// hands the mode to the parts' maker. Only as a type there (TR R0a fix G-2).
const DIAL_TYPE_FILE: &str = "crates/kourier-app/src/app.rs";

fn dial_variant() -> String {
    ["Dial", "Mode::Tele", "gram"].concat()
}

/// Whether the type name `ty` of the flat text is followed by a path (`Ty::X`, `<Ty>::X`): used as a
/// path root it can name a variant or a constructor without the variant's own spelling.
fn type_used_as_a_path(all: &str, ty: &str) -> bool {
    name_positions(all, ty).into_iter().any(|at| {
        all[at + ty.len()..]
            .trim_start_matches(['>', ' '])
            .starts_with("::")
    })
}

/// Whether the type name `ty` of the flat text is the TARGET of an impl: `impl Tr for ty`, `impl ty`,
/// `impl<..> Tr for path::ty<..>`, also behind a `&` or `&mut` (re-check N-2). A trait impl on the
/// foreign type makes `Self::Telegram` a spelling of the variant that no `DialMode::Telegram` check
/// sees, so the type may be implemented for nowhere outside the parts' module.
fn type_is_an_impl_target(all: &str, ty: &str) -> bool {
    let ident = |c: char| c.is_alphanumeric() || c == '_';
    name_positions(all, ty).into_iter().any(|at| {
        let mut before = all[..at].trim_end();
        // The path in front of the name: `a::b::` (`flat` leaves no white space around `::`).
        while let Some(path) = before.strip_suffix("::") {
            before = path.trim_end_matches(ident).trim_end();
        }
        if let Some(rest) = before.strip_suffix('&') {
            before = rest.trim_end();
        } else if let Some(rest) = before.strip_suffix("mut")
            && let Some(rest) = rest.trim_end().strip_suffix('&')
        {
            before = rest.trim_end();
        }
        let word_start = before.trim_end_matches(ident).len();
        matches!(&before[word_start..], "impl" | "for")
    })
}

/// Whether a `macro_rules!` item of the flat text names `ty` in its body.
fn macro_names_the_type(all: &str, ty: &str) -> bool {
    name_positions(all, "macro_rules!")
        .into_iter()
        .any(|at| names(&all[at..at + item_end(&all[at..])], ty))
}

/// Where the dial mode that dials is named. Outside the parts' module the variant is named only in
/// the application's confirmation: in the non-test code of `kourier-app/src/telegram.rs`, at most
/// once (exactly once as soon as `fn dial_confirmation` exists), inside that function; no test names it
/// (a test reads the mode through `DialMode::word`), and nobody imports it by a list holding the
/// variant or `self`, a glob or a rename, nobody gives the type an alias or writes a macro that
/// builds a path from it (`DialMode::$v`).
///
/// The TYPE name (fix G-2, review F2) may stand only in the parts' module, the confirmation's file and
/// `kourier-app/src/app.rs`, which hands the mode to the maker and may name the type only AS a type
/// (a parameter, a field, a return type: never followed by a path, never inside a macro, never the
/// target of an impl: `Self::Telegram` would name the variant past every check; re-check N-2);
/// nowhere in a `tests/` directory or another crate.
fn dial_mode_violations(path: &str, text: &str) -> Vec<String> {
    if path == DIAL_MODE_FILE {
        return Vec::new();
    }
    let mut bad = Vec::new();
    let variant = dial_variant();
    let ty = ["Dial", "Mode"].concat();
    let plain = code(text);
    let all = flat(&plain);
    if use_list_names(&plain, &format!("{ty}::{{"))
        .iter()
        .any(|n| n == "Telegram" || n == "self")
        || names(&all, &format!("{ty}::*"))
        || names(&all, &format!("{ty} as "))
    {
        bad.push(format!(
            "{path} imports the dial mode by a list, a glob or a rename"
        ));
    }
    if type_alias_names(&all, &ty) {
        bad.push(format!("{path} gives the dial mode a type alias"));
    }
    if names(&all, &format!("{ty}::$")) {
        bad.push(format!("{path} builds a path of the dial mode in a macro"));
    }
    let may_name_the_type = [DIAL_CHOOSER_FILE, DIAL_TYPE_FILE].contains(&path);
    if !may_name_the_type && names(&all, &ty) {
        bad.push(format!("{path} names the type {ty}"));
    }
    if may_name_the_type && macro_names_the_type(&all, &ty) {
        bad.push(format!("{path} names the type {ty} in a macro"));
    }
    if type_is_an_impl_target(&all, &ty) {
        bad.push(format!("{path} implements something for the type {ty}"));
    }
    if path == DIAL_TYPE_FILE && type_used_as_a_path(&all, &ty) {
        bad.push(format!(
            "{path} uses {ty} as a path: it may name only the type"
        ));
    }
    if path != DIAL_CHOOSER_FILE {
        if names(&all, &variant) {
            bad.push(format!("{path} names {variant}"));
        }
        return bad;
    }
    let inside = match non_test_code(text) {
        Ok(stripped) => flat(&stripped),
        Err(line) => {
            bad.push(strip_failure(path, line));
            return bad;
        }
    };
    let in_code = name_positions(&inside, &variant);
    if name_positions(&all, &variant).len() != in_code.len() {
        bad.push(format!("{path} names {variant} in test code"));
    }
    if in_code.len() > 1 {
        bad.push(format!("{path} names {variant} {} times", in_code.len()));
    }
    let chooser = name_positions(&inside, DIAL_CHOOSER_FN)
        .first()
        .map(|&start| {
            let body = &inside[start..];
            (start, start + item_end(body))
        });
    // Once the function exists it names the mode, exactly once (until then nothing does).
    if chooser.is_some() && in_code.is_empty() {
        bad.push(format!("{DIAL_CHOOSER_FN} in {path} never names {variant}"));
    }
    for at in in_code {
        if !chooser.is_some_and(|(start, end)| (start..end).contains(&at)) {
            bad.push(format!("{path} names {variant} outside {DIAL_CHOOSER_FN}"));
        }
    }
    bad
}

/// TR R0a D-12 (b): which dialer the application's parts carry is decided in ONE place. Mutant: a
/// second naming in `kourier-app/src/app.rs`.
#[test]
fn the_telegram_dial_mode_is_chosen_in_one_place() {
    let mut bad = Vec::new();
    let mut named_in_the_parts = false;
    for (path, text) in workspace_files() {
        bad.extend(dial_mode_violations(&path, &text));
        if path == DIAL_MODE_FILE {
            named_in_the_parts = names(&flat(&code(&text)), &dial_variant());
        }
    }
    assert!(bad.is_empty(), "{bad:#?}");
    assert!(
        named_in_the_parts,
        "the parts' module does not name the mode"
    );
}

/// The samples of D-12 (b): the allowed shape passes, every other fails. Fix G-2: also a type alias,
/// a `self` in the variant's list (`self as M`), a macro that builds a path from the type, the type
/// named in a `tests/` file or another crate, and the type used as a path in the composition's file.
#[test]
fn the_dial_mode_check_sees_its_samples() {
    let v = dial_variant();
    let ty = ["Dial", "Mode"].concat();
    let app = DIAL_CHOOSER_FILE;
    let good = format!(
        "fn dial_confirmation(debug: bool) -> X {{\n    if debug {{ return {v}; }}\n    Y\n}}\nfn dial_confirmation_as() -> Z {{ Z }}"
    );
    assert!(
        dial_mode_violations(app, &good).is_empty(),
        "the allowed shape fails"
    );
    let outside = format!("fn other() -> X {{ {v} }}\nfn dial_confirmation() -> X {{ Y }}");
    let twice = format!("fn dial_confirmation() -> X {{ if a {{ {v} }} else {{ {v} }} }}");
    let never = "fn dial_confirmation() -> X { Y }".to_owned();
    let in_as =
        format!("fn dial_confirmation() -> X {{ Y }}\nfn dial_confirmation_as() -> X {{ {v} }}");
    let in_tests = format!(
        "fn dial_confirmation() -> X {{ {v} }}\n#[cfg(test)]\nmod tests {{ fn t() {{ let _ = {v}; }} }}"
    );
    let list = format!("use x::{ty}::{{Refuse, Telegram}};");
    let glob = format!("use x::{ty}::*;");
    let rename = format!("use x::{ty} as Mode;");
    let alias = format!("type M = x::modes::{ty};\nfn f() {{ let _ = M::Refuse; }}");
    let alias_pub = format!("pub(crate) type M = Option<{ty}>;");
    let self_as = format!("use x::{ty}::{{self as M}};\nfn f() {{ let _ = M::Refuse; }}");
    let self_plain = format!("use x::{ty}::{{self, Refuse}};");
    let fragment = format!(
        "macro_rules! pick {{ ($v:ident) => {{ x::{ty}::$v }} }}\nfn f() {{ let _ = pick!(Refuse); }}"
    );
    let fragment_spaced = format!("macro_rules! pick {{ ($v:ident) => {{ x::{ty} :: $v }} }}");
    // A macro written with parentheses and a block BEFORE its path: the macro check reads up to the
    // first block's end and stops short of the path, so only the fragment check (`{ty}::$`) sees it.
    let fragment_parens = format!(
        "macro_rules! pick ( ($v:ident) => ( {{ 0 }} x::{ty}::$v ) );\nfn f() {{ other(); }}"
    );
    for (what, sample) in [
        ("outside the function", &outside),
        ("twice", &twice),
        ("never, in a function that exists", &never),
        ("in the _as variant", &in_as),
        ("in a test module", &in_tests),
        ("by a list", &list),
        ("by a glob", &glob),
        ("by a rename", &rename),
        ("through a type alias", &alias),
        ("through a type alias of a wrapper", &alias_pub),
        ("through `self as`", &self_as),
        ("through `self` in a list", &self_plain),
        ("through a macro fragment", &fragment),
        ("through a spaced macro fragment", &fragment_spaced),
        ("through a macro fragment in parentheses", &fragment_parens),
    ] {
        assert!(
            !dial_mode_violations(app, sample).is_empty(),
            "the chooser's file may name the mode {what}"
        );
    }
    // The chooser's file may name the TYPE and the refusing variant (the function builds its answer).
    let types = format!(
        "pub(crate) struct D {{ pub(crate) mode: {ty} }}\nfn f(m: {ty}) -> Option<{ty}> {{ Some({ty}::Refuse) }}\nfn g(m: x::modes::{ty}) {{ m.word(); }}"
    );
    assert!(
        dial_mode_violations(app, &types).is_empty(),
        "the chooser's file may not name the type"
    );
    // Everywhere else, in code and in tests, by any spelling: the variant, the lists, the alias, the
    // macro fragment, and even the type.
    let type_only = format!("fn f(m: {ty}) -> {ty} {{ m }}");
    let type_in_path = format!("fn f() {{ let m = {ty}::Refuse; }}");
    for path in [
        "crates/kourier-app/src/dev.rs",
        "crates/kourier-app/src/restart.rs",
        "crates/kourier-app/tests/it.rs",
        "crates/kourier-app/tests/telegram.rs",
        "crates/kourier/tests/hardening.rs",
        "crates/kourier/src/main.rs",
        "crates/kourier-telegram/src/actor.rs",
        "crates/kourier-telegram/src/supervisor/tests.rs",
        "crates/kourier-telegram/tests/actor.rs",
        "xtask/src/tg_run/mod.rs",
    ] {
        for sample in [
            &format!("let m = {v};"),
            &list,
            &glob,
            &rename,
            &alias,
            &self_as,
            &fragment,
            &type_only,
            &type_in_path,
        ] {
            assert!(
                !dial_mode_violations(path, sample).is_empty(),
                "{path} may name the mode: {sample}"
            );
        }
        assert!(dial_mode_violations(path, &format!("// {v}")).is_empty());
        assert!(
            dial_mode_violations(path, "let w = mode.word(); let refuse = Mode::Refuse;")
                .is_empty()
        );
    }
    // Re-check N-2: a trait implemented for the type is a second chooser (`Self::Telegram` names the
    // variant without its spelling), in the composition's file and in the confirmation's alike.
    let impl_for = format!(
        "trait Pick {{ fn pick() -> Self; }}\nimpl Pick for kourier_telegram::modes::{ty} {{ fn pick() -> Self {{ Self::Telegram }} }}"
    );
    let impl_plain = format!(
        "trait Pick {{ fn pick() -> Self; }}\nimpl Pick for {ty} {{ fn pick() -> Self {{ Self::Refuse }} }}"
    );
    let impl_generic = format!("impl<T> Pick<T> for x::{ty}<T> where T: Y {{ }}");
    let impl_inherent = format!("impl {ty} {{ fn pick() -> Self {{ Self::Refuse }} }}");
    let impl_ref = format!("impl Pick for &x::{ty} {{ }}\nimpl Pick for &mut {ty} {{ }}");
    let impl_spaced = format!("impl Pick\n    for x ::\n    {ty}\n{{\n}}");
    for (file, sample) in [
        (DIAL_TYPE_FILE, &impl_for),
        (DIAL_CHOOSER_FILE, &impl_for),
        (DIAL_TYPE_FILE, &impl_plain),
        (DIAL_CHOOSER_FILE, &impl_plain),
        (DIAL_TYPE_FILE, &impl_generic),
        (DIAL_CHOOSER_FILE, &impl_inherent),
        (DIAL_TYPE_FILE, &impl_ref),
        (DIAL_CHOOSER_FILE, &impl_spaced),
    ] {
        assert!(
            !dial_mode_violations(file, sample).is_empty(),
            "{file} may implement something for the type: {sample}"
        );
    }
    // Naming the type next to an impl, or in a `for` loop that is no impl target, is fine.
    let near_an_impl = format!(
        "impl Pick for Other {{ fn pick(m: {ty}) -> {ty} {{ m }} }}\nfn g(modes: Vec<{ty}>) {{ for m in modes {{ m.word(); }} }}"
    );
    for file in [DIAL_TYPE_FILE, DIAL_CHOOSER_FILE] {
        assert!(
            dial_mode_violations(file, &near_an_impl).is_empty(),
            "{file}: a parameter next to an impl is no impl target"
        );
    }
    // The composition's file names the type, as a type and nothing else.
    let composition = DIAL_TYPE_FILE;
    for fine in [
        format!(
            "pub(crate) struct D {{ pub(crate) mode: {ty} }}\nfn f(m: {ty}) -> Option<{ty}> {{ None }}"
        ),
        format!("fn make(mode: kourier_telegram::modes::{ty}) -> X {{ make(mode) }}"),
        format!("let seen = |mode: {ty}| mode.word();\nlet d = Default::default();"),
        "macro_rules! other { () => { 1 } }\nfn f() { let m = other!(); }".to_owned(),
    ] {
        assert!(
            dial_mode_violations(composition, &fine).is_empty(),
            "the composition's file may not write: {fine}"
        );
    }
    for (what, bad) in [
        (
            "the type as a path",
            format!("fn f() {{ let m = {ty}::Refuse; }}"),
        ),
        (
            "a path through the bracketed type",
            format!("fn f() {{ let m = <{ty}>::default(); }}"),
        ),
        (
            "a path through a generic",
            format!("fn f() {{ let m = Option::<{ty}>::None; }}"),
        ),
        ("the variant", format!("let m = {v};")),
        ("an alias", alias.clone()),
        ("a self list", self_as.clone()),
        ("a macro fragment", fragment.clone()),
        ("a macro fragment in parentheses", fragment_parens.clone()),
        (
            "the type inside a macro",
            format!("macro_rules! m {{ () => {{ let _: {ty}; }} }}"),
        ),
        (
            "an unterminated gated item in the chooser's file",
            "fn dial_confirmation() { }\nfn f() {\n    #[cfg(test)]\n    g(x)\n}".to_owned(),
        ),
    ] {
        let file = if what.contains("chooser") {
            DIAL_CHOOSER_FILE
        } else {
            composition
        };
        assert!(
            !dial_mode_violations(file, &bad).is_empty(),
            "{file} may write ({what}): {bad}"
        );
    }
    // The type in a macro of the chooser's file is refused too.
    assert!(
        !dial_mode_violations(
            DIAL_CHOOSER_FILE,
            &format!("macro_rules! m {{ () => {{ let _: {ty}; }} }}")
        )
        .is_empty()
    );
    assert!(dial_mode_violations(DIAL_MODE_FILE, &format!("let m = {v};")).is_empty());
    assert!(
        dial_mode_violations(
            DIAL_MODE_FILE,
            &format!("type M = {ty}; macro_rules! m {{ () => {{ {ty}::$v }} }}")
        )
        .is_empty(),
        "the parts' module may do what it likes with its own type"
    );
}
