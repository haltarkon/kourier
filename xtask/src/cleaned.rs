//! The source check of LOC's statement (F14, BQ5), run by `cargo xtask deps-check`: a network
//! states `NetworkDescriptor::cleaned_pictures` ("the bytes that leave this PC carry no
//! location") only from a crate whose `src` calls `kourier_media::clean_still`, the one
//! cleaning function that never returns located bytes.
//!
//! The check is lexical: in a crate that never calls `clean_still`, EVERY mention of the field
//! name in a non-test source line is an error unless it is `cleaned_pictures: None` or the
//! field's declaration (`: Option<…>`), so `.replace(…)`, `.get_or_insert_with(…)`, a method
//! `cleaned_pictures(…)` or a string all fail (LOC P7). The core's reads (`.clone()`,
//! `.as_ref()`; [`READERS`]) are the one other allowed mention. Its limits: a struct update
//! (`..other.descriptor()`) copies the field without naming it, and a crate that calls
//! `clean_still` anywhere is trusted for every picture path it has (F14 accepted both as
//! structural; the source is reviewed when a backend gains a send path). Test files are skipped
//! by NAME (`tests.rs`, any file under a `tests` directory), not by
//! `#[cfg(test)]`: a `#[cfg(test)] mod` written inline in a source file is checked like the rest.

use anyhow::Result;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Crates that may state `cleaned_pictures` without calling `clean_still`, with the reason.
pub const EXCEPTIONS: [(&str, &str); 1] = [(
    "kourier-fake",
    "the scripted test network sends nothing off the PC (its outgoing bytes are never \
     processed or transmitted); its Signal-like flavour states Signal's types so the core's \
     verdict runs as on Signal",
)];

/// Crates that read the statement (the core's verdict and plan) and never set it: their reads of
/// the field (`.cleaned_pictures.clone()` / `.as_ref()`) are not statements.
pub const READERS: [&str; 1] = ["kourier-core"];

/// One crate's `src` files: path and text.
pub type Sources = Vec<(PathBuf, String)>;

/// Reads every `crates/<name>/src/**.rs` under `root` and checks them.
pub fn check(root: &Path) -> Result<Vec<String>> {
    let mut crates = BTreeMap::new();
    for entry in std::fs::read_dir(root.join("crates"))? {
        let dir = entry?.path();
        let Some(name) = dir.file_name().and_then(|n| n.to_str()).map(str::to_owned) else {
            continue;
        };
        let mut sources = Vec::new();
        for file in crate::rust_files(&dir.join("src"))? {
            let text = std::fs::read_to_string(&file)?;
            sources.push((file, text));
        }
        crates.insert(name, sources);
    }
    Ok(check_sources(&crates, &EXCEPTIONS))
}

/// The check over sources in memory: each mention of `cleaned_pictures` other than `: None`, the
/// field's declaration and a [`READERS`] crate's read, outside a test file, must stand in a crate
/// whose sources call `kourier_media::clean_still`, or in an excepted crate.
pub fn check_sources(
    crates: &BTreeMap<String, Sources>,
    exceptions: &[(&str, &str)],
) -> Vec<String> {
    let mut errors = Vec::new();
    for (name, sources) in crates {
        if exceptions.iter().any(|(n, _)| n == name) {
            continue;
        }
        let calls = sources
            .iter()
            .any(|(_, text)| code_lines(text).any(|l| l.contains("kourier_media::clean_still")));
        if calls {
            continue;
        }
        for (path, text) in sources.iter().filter(|(p, _)| !is_test_file(p)) {
            for (i, line) in text.lines().enumerate() {
                if states_cleaning(line, READERS.contains(&name.as_str())) {
                    errors.push(format!(
                        "{}:{}: {name} states `cleaned_pictures` but never calls \
                         `kourier_media::clean_still` (LOC F14)",
                        path.display(),
                        i + 1
                    ));
                }
            }
        }
    }
    errors
}

fn code_lines(text: &str) -> impl Iterator<Item = &str> {
    text.lines().filter(|l| !l.trim_start().starts_with("//"))
}

fn is_test_file(path: &Path) -> bool {
    path.file_name().is_some_and(|n| n == "tests.rs")
        || path.components().any(|c| c.as_os_str() == "tests")
}

/// Whether a line mentions `cleaned_pictures` in any way but `cleaned_pictures: None`, the
/// field's own declaration (`: Option<…>`) and, when `reader` is set, a read (`.clone()`,
/// `.as_ref()`). A comment line has none.
fn states_cleaning(line: &str, reader: bool) -> bool {
    const NAME: &str = "cleaned_pictures";
    if line.trim_start().starts_with("//") {
        return false;
    }
    let mut rest = line;
    while let Some(pos) = rest.find(NAME) {
        let after = rest[pos + NAME.len()..].trim_start();
        let allowed = if let Some(value) = after.strip_prefix(':') {
            let value = value.trim_start();
            value.starts_with("Option<")
                || value.strip_prefix("None").is_some_and(|tail| {
                    !tail.starts_with(|c: char| c.is_alphanumeric() || c == '_' || c == '.')
                })
        } else {
            reader && (after.starts_with(".clone()") || after.starts_with(".as_ref()"))
        };
        if !allowed {
            return true;
        }
        rest = &rest[pos + NAME.len()..];
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(crates: &[(&str, &str, &str)]) -> BTreeMap<String, Sources> {
        let mut out: BTreeMap<String, Sources> = BTreeMap::new();
        for (name, file, text) in crates {
            out.entry((*name).to_owned()).or_default().push((
                PathBuf::from(format!("crates/{name}/src/{file}")),
                (*text).to_owned(),
            ));
        }
        out
    }

    const STATES: &str = "    cleaned_pictures: Some(CleanedPictures::stills(T)),\n";
    const CALLS: &str = "    match kourier_media::clean_still(bytes, LEVEL) {\n";

    #[test]
    fn a_statement_beside_the_call_passes() {
        let t = tree(&[("net-a", "lib.rs", STATES), ("net-a", "send.rs", CALLS)]);
        assert_eq!(check_sources(&t, &[]), Vec::<String>::new());
    }

    #[test]
    fn a_planted_crate_that_states_without_the_call_fails() {
        let t = tree(&[
            ("net-a", "lib.rs", STATES),
            ("net-b", "lib.rs", "    d.cleaned_pictures = Some(x);\n"),
            (
                "net-c",
                "flavor.rs",
                "    cleaned_pictures: on.then(|| x),\n",
            ),
        ]);
        let errors = check_sources(&t, &[]);
        assert_eq!(errors.len(), 3, "{errors:?}");
        assert!(errors[0].starts_with("crates/net-a/src/lib.rs:1: net-a"));
        // A call only in a comment does not count.
        let t = tree(&[
            ("net-a", "lib.rs", STATES),
            ("net-a", "send.rs", "// kourier_media::clean_still(x)\n"),
        ]);
        assert_eq!(check_sources(&t, &[]).len(), 1);
    }

    #[test]
    fn none_declarations_comments_tests_and_the_exception_pass() {
        let t = tree(&[
            ("net-a", "lib.rs", "    cleaned_pictures: None,\n"),
            (
                "net-a",
                "x.rs",
                "    pub cleaned_pictures: Option<CleanedPictures>,\n",
            ),
            (
                "net-a",
                "y.rs",
                "    // cleaned_pictures: Some(..) through clean_still\n",
            ),
            (
                "net-a",
                "tests/z.rs",
                "    assert!(d.cleaned_pictures == Some(x));\n",
            ),
            ("net-a", "send/tests.rs", STATES),
            ("fake", "flavor.rs", STATES),
        ]);
        assert_eq!(check_sources(&t, &[("fake", "why")]), Vec::<String>::new());
    }

    #[test]
    fn every_other_mention_in_a_crate_without_the_call_fails() {
        // LOC P7: not only `:` and `=`; any way of setting (or touching) the field counts.
        for line in [
            "    d.cleaned_pictures.replace(CleanedPictures::stills(T));\n",
            "    d.cleaned_pictures.get_or_insert_with(|| x);\n",
            "    std::mem::swap(&mut d.cleaned_pictures, &mut other);\n",
            "    let x = d.cleaned_pictures.clone();\n",
            "    cleaned_pictures: None.or(Some(x)),\n",
            "    cleaned_pictures:\n",
            "    d.cleaned_pictures = None;\n",
        ] {
            let t = tree(&[("net-a", "lib.rs", line)]);
            assert_eq!(check_sources(&t, &[]).len(), 1, "{line}");
        }
        // A plain `: None` (also written `: None,}`) and a declaration pass.
        for line in [
            "    cleaned_pictures: None,\n",
            "    cleaned_pictures : None }\n",
            "    cleaned_pictures: Option<CleanedPictures>,\n",
        ] {
            let t = tree(&[("net-a", "lib.rs", line)]);
            assert_eq!(check_sources(&t, &[]), Vec::<String>::new(), "{line}");
        }
    }

    #[test]
    fn the_core_may_read_but_not_set_it() {
        let read = "    let c = n.descriptor().cleaned_pictures.clone();\n";
        let t = tree(&[("kourier-core", "runtime.rs", read)]);
        assert_eq!(check_sources(&t, &[]), Vec::<String>::new());
        // Only the core: the same read in a backend crate is a mention.
        let t = tree(&[("net-a", "runtime.rs", read)]);
        assert_eq!(check_sources(&t, &[]).len(), 1);
        // The core does not set it either.
        let t = tree(&[(
            "kourier-core",
            "runtime.rs",
            "    d.cleaned_pictures.replace(x);\n",
        )]);
        assert_eq!(check_sources(&t, &[]).len(), 1);
    }

    #[test]
    fn the_tree_passes_and_the_fake_is_the_named_exception() {
        let errors = check(&crate::root()).expect("reads the tree");
        assert_eq!(errors, Vec::<String>::new());
        // Without its exception the fake fails: the check sees its statement.
        let mut crates = BTreeMap::new();
        let dir = crate::root().join("crates/kourier-fake/src");
        let mut sources = Vec::new();
        for file in crate::rust_files(&dir).unwrap() {
            let text = std::fs::read_to_string(&file).unwrap();
            sources.push((file, text));
        }
        crates.insert("kourier-fake".to_owned(), sources);
        assert!(!check_sources(&crates, &[]).is_empty());
    }
}
