//! `cargo xtask emoji`: the emoji table of the pickers from the vendored Unicode data
//! (`third_party/unicode-emoji/VENDORED.md`) into `crates/kourier-viewmodel/src/emoji/table.rs`.
//!
//! One row per fully-qualified emoji of the file `emoji-test.txt`, in its order, up to
//! [`EMOJI_VERSION_CAP`] (the newest Emoji version the system font draws), without the `Component`
//! group and without skin-tone and hair variants. The row holds the glyph, the English and the
//! Ukrainian name (CLDR `tts`) and the folded search words of both languages (names and keywords),
//! as ONE string of tab-separated rows and an array of the rows' offsets: static read-only data,
//! nothing to parse or allocate at start. A unit test fails when the checked-in table is not what
//! this writes now (the `i18n check` pattern).

use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

#[path = "../../crates/kourier-viewmodel/src/emoji/fold.rs"]
mod fold;

/// The newest Emoji version the installed Noto Color Emoji (2.047) draws; see VENDORED.md.
pub const EMOJI_VERSION_CAP: (u32, u32) = (16, 0);

const DATA: &str = "third_party/unicode-emoji";
const TABLE: &str = "crates/kourier-viewmodel/src/emoji/table.rs";

pub fn emoji(args: &[String]) -> Result<()> {
    if !args.is_empty() {
        bail!("usage: cargo xtask emoji");
    }
    let root = crate::root();
    let text = generate(&root)?;
    std::fs::write(root.join(TABLE), text).context("write the table")?;
    println!("wrote {TABLE}");
    Ok(())
}

/// The annotations of one language: the emoji (U+FE0F removed) to its name and its keywords.
#[derive(Default)]
struct Annotations {
    names: BTreeMap<String, String>,
    keywords: BTreeMap<String, Vec<String>>,
}

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

fn read_annotations(dir: &Path, lang: &str) -> Result<Annotations> {
    let mut out = Annotations::default();
    // `annotations` first: what it holds wins over `annotationsDerived`.
    for file in [
        format!("annotations-{lang}.xml"),
        format!("annotationsDerived-{lang}.xml"),
    ] {
        let text = std::fs::read_to_string(dir.join(&file)).with_context(|| file.clone())?;
        for line in text.lines() {
            let line = line.trim();
            let Some(rest) = line.strip_prefix("<annotation cp=\"") else {
                continue;
            };
            let Some((cp, rest)) = rest.split_once('"') else {
                continue;
            };
            let (is_tts, rest) = match rest.strip_prefix(" type=\"tts\">") {
                Some(r) => (true, r),
                None => match rest.strip_prefix('>') {
                    Some(r) => (false, r),
                    None => continue,
                },
            };
            let Some(body) = rest.strip_suffix("</annotation>") else {
                continue;
            };
            let key = unescape(cp).replace('\u{fe0f}', "");
            let body = unescape(body);
            if is_tts {
                out.names.entry(key).or_insert(body);
            } else {
                out.keywords
                    .entry(key)
                    .or_insert_with(|| body.split('|').map(|k| k.trim().to_owned()).collect());
            }
        }
    }
    Ok(out)
}

/// The search words of an emoji for one language: the folded words of its name and keywords, each once.
fn search_words(name: Option<&String>, keywords: Option<&Vec<String>>) -> String {
    let mut seen: Vec<String> = Vec::new();
    for text in name.into_iter().chain(keywords.into_iter().flatten()) {
        for w in fold::words(text) {
            if !seen.contains(&w) {
                seen.push(w);
            }
        }
    }
    seen.join(" ")
}

fn is_variant(cps: &[u32]) -> bool {
    cps.iter()
        .any(|c| (0x1F3FB..=0x1F3FF).contains(c) || (0x1F9B0..=0x1F9B3).contains(c))
}

/// The table's source text for the vendored data under `root`.
pub fn generate(root: &Path) -> Result<String> {
    let dir = root.join(DATA);
    let en = read_annotations(&dir, "en")?;
    let uk = read_annotations(&dir, "uk")?;
    let test = std::fs::read_to_string(dir.join("emoji-test.txt")).context("emoji-test.txt")?;

    let mut groups: Vec<(String, usize)> = Vec::new();
    let mut group = String::new();
    let mut rows: Vec<String> = Vec::new();
    for line in test.lines() {
        if let Some(g) = line.strip_prefix("# group: ") {
            group = g.trim().to_owned();
            continue;
        }
        let Some((cps, rest)) = line.split_once(';') else {
            continue;
        };
        let Some((status, comment)) = rest.split_once('#') else {
            continue;
        };
        if status.trim() != "fully-qualified" || group == "Component" {
            continue;
        }
        let cps: Vec<u32> = cps
            .split_whitespace()
            .map(|c| u32::from_str_radix(c, 16).context("code point"))
            .collect::<Result<_>>()?;
        // The comment: `<glyph> E16.0 <name>`.
        let mut parts = comment.split_whitespace();
        let (Some(glyph), Some(version), Some(english)) =
            (parts.next(), parts.next(), parts.next())
        else {
            bail!("bad comment: {line}");
        };
        let _ = english;
        let version = version.trim_start_matches('E');
        let (major, minor) = version.split_once('.').context("version")?;
        let version = (major.parse::<u32>()?, minor.parse::<u32>()?);
        if version > EMOJI_VERSION_CAP || is_variant(&cps) {
            continue;
        }
        if groups.last().map(|g| &g.0) != Some(&group) {
            groups.push((group.clone(), rows.len()));
        }
        let key: String = glyph.replace('\u{fe0f}', "");
        let name_en = en
            .names
            .get(&key)
            .with_context(|| format!("no English name for {line}"))?;
        let name_uk = uk.names.get(&key).unwrap_or(name_en);
        let sen = search_words(Some(name_en), en.keywords.get(&key));
        let suk = if uk.names.contains_key(&key) {
            search_words(Some(name_uk), uk.keywords.get(&key))
        } else {
            String::new()
        };
        rows.push(format!("{glyph}\t{name_en}\t{name_uk}\t{sen}\t{suk}\n"));
    }

    let mut out = String::new();
    out.push_str(
        "// @generated by `cargo xtask emoji` from third_party/unicode-emoji (see its VENDORED.md): do not edit.\n\
         //! The emoji table of the pickers: static read-only data. A row of `BLOB` is\n\
         //! `glyph TAB English name TAB Ukrainian name TAB English search words TAB Ukrainian search words`.\n\n",
    );
    let _ = writeln!(
        out,
        "/// The newest Emoji version the table holds (the system font's: VENDORED.md).\npub const EMOJI_VERSION_CAP: &str = \"{}.{}\";\n",
        EMOJI_VERSION_CAP.0, EMOJI_VERSION_CAP.1
    );
    let _ = writeln!(
        out,
        "/// The number of emoji.\npub const COUNT: usize = {};\n",
        rows.len()
    );
    let _ = writeln!(
        out,
        "/// The groups in order: the CLDR/Unicode name and the index of the first emoji.\n#[rustfmt::skip]\npub static GROUPS: [(&str, u16); {}] = [",
        groups.len()
    );
    for (name, first) in &groups {
        let _ = writeln!(out, "    (\"{name}\", {first}),");
    }
    out.push_str("];\n\n");
    let _ = writeln!(
        out,
        "/// Where each row starts in `BLOB`, and the length of the whole.\n#[rustfmt::skip]\npub static OFFS: [u32; {}] = [",
        rows.len() + 1
    );
    let mut at = 0usize;
    let mut line = String::from("   ");
    for r in &rows {
        let _ = write!(line, " {at},");
        if line.len() > 90 {
            out.push_str(&line);
            out.push('\n');
            line = String::from("   ");
        }
        at += r.len();
    }
    let _ = write!(line, " {at},");
    out.push_str(&line);
    out.push_str("\n];\n\n");
    // The rows' positions ordered by glyph: a lookup by binary search touches a dozen rows, not the table.
    let mut by_glyph: Vec<usize> = (0..rows.len()).collect();
    by_glyph.sort_by_key(|&i| rows[i].split('\t').next().unwrap_or_default().to_owned());
    let _ = writeln!(
        out,
        "/// The positions of the rows ordered by glyph (bytes), for `emoji::index_of`.\n#[rustfmt::skip]\npub static BY_GLYPH: [u16; {}] = [",
        rows.len()
    );
    let mut line = String::from("   ");
    for i in &by_glyph {
        let _ = write!(line, " {i},");
        if line.len() > 90 {
            out.push_str(&line);
            out.push('\n');
            line = String::from("   ");
        }
    }
    out.push_str(&line);
    out.push_str("\n];\n\n");
    out.push_str("/// The rows.\n#[rustfmt::skip]\npub static BLOB: &str = r##\"");
    for r in &rows {
        out.push_str(r);
    }
    out.push_str("\"##;\n");
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_checked_in_table_is_what_the_generator_writes() {
        let root = crate::root();
        let now = generate(&root).expect("generate");
        let kept = std::fs::read_to_string(root.join(TABLE)).expect("table");
        assert!(
            now == kept,
            "{TABLE} is not what `cargo xtask emoji` writes now: run it and commit the result"
        );
    }

    #[test]
    fn variants_and_the_component_group_are_left_out() {
        assert!(is_variant(&[0x1F44B, 0x1F3FB]));
        assert!(is_variant(&[0x1F9D1, 0x200D, 0x1F9B0]));
        assert!(!is_variant(&[0x1F600]));
    }
}
