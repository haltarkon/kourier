//! The names the tool and the app agree on. The app's constants are read out of its source; the
//! tool's copies must equal them (a keeper root or a list file the two name differently would never
//! meet: the list the tool writes would be a file the app never reads).

use crate::threema_run::{ALLOW_FILE, KEEPER_DIR};

/// The value of `const NAME: &str = "value";` (any `pub`/`pub(crate)`) in `text`; exactly one such
/// line, outside a comment.
fn string_const(text: &str, name: &str) -> String {
    let found: Vec<String> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.starts_with("//"))
        .filter_map(|l| {
            let l = l
                .strip_prefix("pub(crate) ")
                .or_else(|| l.strip_prefix("pub "))
                .unwrap_or(l);
            let rest = l.strip_prefix("const ")?.strip_prefix(name)?;
            let value = rest.trim_start().strip_prefix(": &str = \"")?;
            Some(value.split('"').next()?.to_owned())
        })
        .collect();
    assert_eq!(found.len(), 1, "{name} is defined {} times", found.len());
    found.into_iter().next().expect("one")
}

fn source(file: &str) -> String {
    std::fs::read_to_string(crate::root().join(file)).unwrap_or_else(|e| panic!("{file}: {e}"))
}

/// Mutant: the tool's keeper directory or the list's file name renamed.
#[test]
fn the_keeper_root_and_the_list_file_match_the_apps() {
    let app = source("crates/kourier-app/src/threema.rs");
    assert_eq!(
        string_const(&app, "RUN_DIR"),
        KEEPER_DIR,
        "the app's RUN_DIR"
    );
    assert_eq!(
        string_const(&app, "ALLOW_FILE"),
        ALLOW_FILE,
        "the app's ALLOW_FILE"
    );
}

/// Mutant: the variable that holds the app to this tool's rules renamed: every run would start
/// with Threema on and any ID allowed.
#[test]
fn the_run_tools_variable_matches_the_apps() {
    let app = source("crates/kourier-app/src/prefs.rs");
    assert_eq!(
        string_const(&app, "THREEMA_RUN_TOOL_ENV"),
        crate::threema_run::launch::RUN_TOOL_ENV,
        "the app's THREEMA_RUN_TOOL_ENV"
    );
}

/// Mutant: the tool's own directory in a profile renamed: the app would no longer see a run
/// profile started by hand as one, and a linked account there would connect (`ThreemaStart::Shut`).
#[test]
fn the_run_tools_directory_matches_the_apps() {
    let app = source("crates/kourier-app/src/prefs.rs");
    assert_eq!(
        string_const(&app, "THREEMA_RUN_TOOL_DIR"),
        crate::threema_run::TOOL_DIR,
        "the app's THREEMA_RUN_TOOL_DIR"
    );
}

/// The parser of the test finds one definition and nothing in a comment.
#[test]
fn the_constant_reader_reads_one_definition() {
    let text = "/// const A: &str = \"no\";\npub(crate) const A: &str = \"yes\";\nconst AB: &str = \"other\";\n#[cfg(x)]\nconst B: &str = \"b\";\n";
    assert_eq!(string_const(text, "A"), "yes");
    assert_eq!(string_const(text, "B"), "b");
}
