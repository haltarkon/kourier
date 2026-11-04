//! X16: the names the tool and the app agree on. The app's constants are read out of its source;
//! the tool's copies must equal them (the R0 precondition: a keeper root or a dial file the two
//! name differently would never meet).

use crate::tg_run::app_names::{DIAL_DIR, DIAL_ENV, DIAL_FILE, REHEARSAL_ENV, TELEGRAM_ENV};
use crate::tg_run::{API_FILE, KEEPER_DIR, TOOL_DIR};

/// The value of `const NAME: &str = "value";` (any `pub`/`pub(crate)`, any attribute line before
/// it) in `text`; exactly one such line, outside a comment.
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

/// Mutant: the tool's keeper directory renamed.
#[test]
fn the_keeper_root_and_the_dial_names_match_the_apps() {
    let app = source("crates/kourier-app/src/telegram.rs");
    for (name, ours) in [
        ("RUN_DIR", KEEPER_DIR),
        ("API_FILE", API_FILE),
        ("TELEGRAM_ENV", TELEGRAM_ENV),
        ("DIAL_ENV", DIAL_ENV),
        ("DIAL_DIR", DIAL_DIR),
        ("DIAL_FILE", DIAL_FILE),
    ] {
        assert_eq!(string_const(&app, name), ours, "the app's {name}");
    }
    let dev = source("crates/kourier-app/src/dev.rs");
    assert_eq!(
        string_const(&dev, "REHEARSAL_ENV"),
        REHEARSAL_ENV,
        "the app's REHEARSAL_ENV"
    );
    // The tool's own directory in a profile is the one the dial file lives in.
    assert_eq!(TOOL_DIR, DIAL_DIR);
}

/// The parser of the test finds one definition and nothing in a comment.
#[test]
fn the_constant_reader_reads_one_definition() {
    let text = "/// const A: &str = \"no\";\npub(crate) const A: &str = \"yes\";\nconst AB: &str = \"other\";\n#[cfg(x)]\nconst B: &str = \"b\";\n";
    assert_eq!(string_const(text, "A"), "yes");
    assert_eq!(string_const(text, "B"), "b");
}
