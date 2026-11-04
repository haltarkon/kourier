//! The name a saved file is offered under (F3, architecture §8.6): the sender's file name
//! cleaned, or, for a picture, a video or a voice note that came without one,
//! `<sender> <date> <time>.<extension>` (`Ari 2026-10-13 18.30.05.jpg`; an album's n-th gets
//! ` 2`, ` 3`). The same cleaner and the same byte budget serve every name the app writes into
//! a user's folder (the Markdown file of a message, M2b, takes them too).
//!
//! The sender's string is UNTRUSTED: nothing checks it but "not empty and at most 255 bytes". The
//! rules ([`clean_name`]), in this order:
//!
//! - every control character goes (a tab, a line feed and the other white space become a space
//!   first); so does U+FFFC;
//! - what hides in an address goes too: the invisible and bidi controls whatever stands beside
//!   them, a joiner out of its place judged WITH the neighbours that stay ([`hides_in_link`] is the
//!   one rule of the links). A Persian word keeps its zero-width non-joiner and an Indic conjunct
//!   its joiner after the virama; `exam` U+200D `ple` loses it;
//! - `/ \ : * ? " < > |` become `_` (a copy to a USB stick or a Windows share keeps working);
//! - white space is collapsed to one space, spaces and dots are trimmed at both ends (so a name
//!   never starts with a dot, which would hide the file, nor ends with one, which Windows
//!   drops);
//! - a leading `-` (an option to every tool that gets the name unquoted) and a leading `~`
//!   (the shell expands `~name`; a WhatsApp push name arrives as `~Ann`) become `_`;
//! - a stem that is a Windows device name (`CON`, `PRN`, `AUX`, `NUL`, `COM0` to `COM9`, `LPT0`
//!   to `LPT9` and the superscript forms `COM¹ COM² COM³ LPT¹ LPT² LPT³`, with or without an
//!   extension) gets `_` in front;
//! - an empty result is the generic word ([`generic_stem`]);
//! - the whole name is at most [`MAX_NAME_BYTES`] BYTES (a Cyrillic letter is two; eCryptfs
//!   allows about 143): the stem is cut at a grapheme boundary, never inside a cluster, a joiner
//!   the cut leaves at its end goes with it, and the extension, the date and the time are never cut.
//!
//! The cleaner is idempotent: the cleaned name cleans to itself (a proptest holds it).
//!
//! The extension rule E2 ([`suggested_name`]): the content is the judge only where it is
//! precise: a picture (JPEG, PNG, GIF, WebP, BMP), PDF, ZIP, FLAC, WAV or MP3 that a name calls
//! something else gets the right extension added (`photo.pdf` that is a PNG is offered as
//! `photo.pdf.png`). Never for containers: the sniffer calls every EBML file `video/webm` and
//! every unknown `ftyp` brand `video/mp4`, so a `.mkv`, `.mov`, `.m4a` or `.3gp` is left as it
//! is. An extension the table does not know (`.docx` is a ZIP, so are `.apk` and `.epub`) is
//! left as it is too.
//!
//! Names enter through [`unmarked_name`](crate::notify::unmarked_name): a WhatsApp push name
//! is `~Ann`, and `~Ann 2026-….jpg` is a shell trap.
//!
//! Log safety (§9.1): the results are names. Never log them.

use unicode_segmentation::UnicodeSegmentation;

use crate::i18n::trc;
use crate::notify::unmarked_name;
use crate::richtext::{hides_in_link, is_invisible_or_bidi};
use crate::time::{Calendar, civil_from_days};
use kourier_core::facade::{extension_for_mime, mime_from_extension};

/// The longest name the app offers, in bytes (most Linux file systems allow 255; eCryptfs about
/// 143; a clash adds ` (n)` and a partial file's name lives beside it).
pub const MAX_NAME_BYTES: usize = 140;

/// The longest extension ([`split_extension`] takes as one), in bytes.
const MAX_EXTENSION_BYTES: usize = 16;

/// The classes of content the extension rule E2 trusts: `(MIME type the sniffer reports,
/// extension)`. Containers are not here on purpose.
const PRECISE: [(&str, &str); 10] = [
    ("image/jpeg", "jpg"),
    ("image/png", "png"),
    ("image/gif", "gif"),
    ("image/webp", "webp"),
    ("image/bmp", "bmp"),
    ("application/pdf", "pdf"),
    ("application/zip", "zip"),
    ("audio/flac", "flac"),
    ("audio/wav", "wav"),
    ("audio/mpeg", "mp3"),
];

/// The word a file is called when its name came to nothing (an empty name, `...`, only
/// controls).
pub fn generic_stem() -> String {
    trc(
        "@item:intext file name offered when the sender gave none",
        "Attachment",
    )
    .into_owned()
}

/// What a saved file is called, for [`suggested_name`].
#[derive(Clone, Copy, Debug)]
pub struct NameFacts<'a> {
    /// The file name the sender gave (stored as it came), if any.
    pub file_name: Option<&'a str>,
    /// Who sent the message, as the chat shows the name (an own message: the account's profile
    /// name). A WhatsApp push name may carry its `~` mark; it is dropped here. Empty: no
    /// sender in the name.
    pub sender: &'a str,
    /// When the message was sent (ms since the Unix epoch).
    pub sent_at_ms: i64,
    /// The file's place among the files of its message, from 1 (an album's n-th picture; the
    /// second gets ` 2`), whether or not the others are on this PC yet: a picture keeps its
    /// name when its neighbours are downloaded later.
    pub position: usize,
    /// The MIME type the message states (the sender's claim).
    pub mime: Option<&'a str>,
    /// What the content proved ([`Query::SniffAttachment`](kourier_core::facade::Query)).
    pub sniffed: Option<&'a str>,
}

/// The name to offer in the dialog.
pub fn suggested_name(facts: &NameFacts<'_>, calendar: &dyn Calendar) -> String {
    match facts
        .file_name
        .map(split_clean)
        .filter(|p| !p.stem.is_empty())
    {
        Some(parts) => assemble(&correct_extension(parts, facts.sniffed)),
        None => nameless_name(facts, calendar),
    }
}

/// `<sender> <date> <time>[ n].<extension>`; the date and the time in the calendar's local
/// time, digits only (a `:` is forbidden on FAT and SMB).
fn nameless_name(facts: &NameFacts<'_>, calendar: &dyn Calendar) -> String {
    let (date, time) = stamp(facts.sent_at_ms, calendar);
    let mut tail = format!("{date} {time}");
    if facts.position > 1 {
        tail.push_str(&format!(" {}", facts.position));
    }
    let head = lead(scrub(unmarked_name(facts.sender)));
    let ext = nameless_extension(facts.mime, facts.sniffed);
    let room = MAX_NAME_BYTES
        .saturating_sub(tail.len() + ext.as_ref().map_or(0, |e| e.len() + 1))
        .saturating_sub(1);
    let head = cut(&head, room);
    let stem = if head.is_empty() {
        tail
    } else {
        format!("{head} {tail}")
    };
    // (Cleaned as a whole: a sender called `COM1.x` would make a device name of the stem.)
    clean_name(&join(&stem, ext.as_deref()))
}

/// The extension of a file with no name: the right one when the content proves a precise class;
/// else the one of the stated type; else the one of whatever the content looks like; else none.
fn nameless_extension(mime: Option<&str>, sniffed: Option<&str>) -> Option<String> {
    if let Some(ext) = sniffed.and_then(precise_extension) {
        return Some(ext.to_owned());
    }
    mime.and_then(extension_for_mime)
        .or_else(|| sniffed.and_then(extension_for_mime))
        .map(str::to_owned)
}

/// The extension of the precise class `mime`, when it is one.
fn precise_extension(mime: &str) -> Option<&'static str> {
    PRECISE
        .iter()
        .find(|(m, _)| m.eq_ignore_ascii_case(mime))
        .map(|(_, e)| *e)
}

/// E2: the content is trusted only where it is precise ([`PRECISE`]). Its extension is ADDED
/// (the name's own stays, in the stem) when the name has none or says another type the table
/// knows. An extension the table does not know is left alone.
fn correct_extension(parts: Parts, sniffed: Option<&str>) -> Parts {
    let Some(proved) = sniffed else {
        return parts;
    };
    let Some(right) = precise_extension(proved) else {
        return parts;
    };
    match &parts.ext {
        None => Parts {
            stem: parts.stem,
            ext: Some(right.to_owned()),
        },
        Some(ext) => match mime_from_extension(ext) {
            // The name says a type the table knows, and not this one.
            Some(says) if !says.eq_ignore_ascii_case(proved) => Parts {
                stem: format!("{}.{ext}", parts.stem),
                ext: Some(right.to_owned()),
            },
            // The same type, or an extension nobody can judge (`.docx` is a ZIP).
            _ => parts,
        },
    }
}

/// `name` as it can be written into a folder: the rules of the module, the extension kept
/// whole. Use it for a name that has no other source of the date, the sender or the content
/// (the Markdown file of M2b assembles its own parts and cleans each).
pub fn clean_name(raw: &str) -> String {
    let mut name = assemble(&split_clean(raw));
    // A cut can make the tail of a name an extension (the space that kept it from being one went
    // with the cut-off words) and a trim moves an edge: the result is cleaned again until it holds
    // still, so the cleaned name of a cleaned name is itself (what the dialog was offered is what
    // is written, whatever cleans it once more). Each round can only shorten the name.
    for _ in 0..CLEAN_ROUNDS {
        let again = assemble(&split_clean(&name));
        if again == name {
            break;
        }
        name = again;
    }
    name
}

/// The most rounds [`clean_name`] goes on cleaning; one more is needed in a few odd names only.
const CLEAN_ROUNDS: usize = 8;

/// `name` with ` (n)` in front of its extension (`report.pdf` -> `report (1).pdf`): the next
/// name to try when a file of that name is in the folder (several files saved into one folder
/// never replace one another). The number counts from 1. The name stays within the byte budget.
pub fn with_number(name: &str, n: u32) -> String {
    let parts = split_clean(name);
    let suffix = format!(" ({n})");
    let ext_len = parts.ext.as_ref().map_or(0, |e| e.len() + 1);
    let room = MAX_NAME_BYTES.saturating_sub(suffix.len() + ext_len);
    let stem = format!("{}{suffix}", cut(&parts.stem, room));
    join(&stem, parts.ext.as_deref())
}

/// A name in its two parts, both cleaned.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Parts {
    stem: String,
    ext: Option<String>,
}

/// Cleans `raw` and splits off its extension.
fn split_clean(raw: &str) -> Parts {
    let name = lead(scrub(raw));
    let (stem, ext) = split_extension(&name);
    let stem = reserved(stem);
    Parts {
        stem,
        ext: ext.map(str::to_owned),
    }
}

/// The parts as one name, cut to the budget.
fn assemble(parts: &Parts) -> String {
    let ext_len = parts.ext.as_ref().map_or(0, |e| e.len() + 1);
    let stem = cut(&parts.stem, MAX_NAME_BYTES.saturating_sub(ext_len));
    let stem = if stem.is_empty() {
        cut(&generic_stem(), MAX_NAME_BYTES.saturating_sub(ext_len))
    } else {
        stem
    };
    join(&stem, parts.ext.as_deref())
}

fn join(stem: &str, ext: Option<&str>) -> String {
    match ext {
        Some(ext) => format!("{stem}.{ext}"),
        None => stem.to_owned(),
    }
}

/// Characters, white space, hiding characters and the characters of a path or a Windows name;
/// spaces collapsed; spaces and dots trimmed at both ends.
fn scrub(raw: &str) -> String {
    // What goes whatever its neighbours are goes FIRST, so a joiner is judged by the neighbours
    // that stay: one kept for a neighbour that then went (U+FFFC, a control, a hiding character)
    // would stand out of its place in the result, and the cleaner would not be idempotent.
    let chars: Vec<char> = raw
        .chars()
        .filter(|&c| {
            c.is_whitespace() || !(c.is_control() || c == '\u{FFFC}' || is_invisible_or_bidi(c))
        })
        .collect();
    let mut out = String::with_capacity(raw.len());
    for (i, &c) in chars.iter().enumerate() {
        let prev = i.checked_sub(1).map(|j| chars[j]);
        let next = chars.get(i + 1).copied();
        let c = if c.is_whitespace() {
            ' '
        } else if hides_in_link(prev, c, next) {
            continue;
        } else if matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') {
            '_'
        } else {
            c
        };
        if c == ' ' && out.ends_with(' ') {
            continue;
        }
        out.push(c);
    }
    out.trim_matches(|c| c == ' ' || c == '.').to_owned()
}

/// A leading `-` or `~` becomes `_`.
fn lead(name: String) -> String {
    match name.chars().next() {
        Some('-' | '~') => format!("_{}", &name[1..]),
        _ => name,
    }
}

/// `name` split at its last dot when what follows is an extension: not empty, at most
/// [`MAX_EXTENSION_BYTES`], no space, and a stem before it.
fn split_extension(name: &str) -> (&str, Option<&str>) {
    match name.rsplit_once('.') {
        Some((stem, ext))
            if !stem.is_empty()
                && !ext.is_empty()
                && ext.len() <= MAX_EXTENSION_BYTES
                && !ext.contains(' ') =>
        {
            (stem, Some(ext))
        }
        _ => (name, None),
    }
}

/// A Windows device name as the stem (or as the part of it before the first dot, which is where
/// Windows judges it) gets `_` in front.
fn reserved(stem: &str) -> String {
    let head = stem.split('.').next().unwrap_or("").trim_end();
    let upper = head.to_ascii_uppercase();
    // Microsoft's list ("Naming Files, Paths, and Namespaces"): COM and LPT with 0 to 9 and with
    // the superscripts 1, 2 and 3.
    let device = matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ["COM", "LPT"].iter().any(|p| {
            upper.strip_prefix(p).is_some_and(|n| {
                matches!(n.as_bytes(), [b'0'..=b'9']) || matches!(n, "\u{B9}" | "\u{B2}" | "\u{B3}")
            })
        });
    if device {
        format!("_{stem}")
    } else {
        stem.to_owned()
    }
}

/// At most `room` bytes of `text`, ending at a grapheme boundary and without a trailing joiner,
/// space or dot.
fn cut(text: &str, room: usize) -> String {
    let mut out = String::new();
    for grapheme in text.graphemes(true) {
        if out.len() + grapheme.len() > room {
            break;
        }
        out.push_str(grapheme);
    }
    // A joiner kept for a neighbour the cut removed would end the stem out of its place (and
    // `clean_name` of the result would drop it: the cleaner must be idempotent).
    while out.ends_with(['\u{200C}', '\u{200D}']) {
        out.pop();
    }
    out.trim_end_matches([' ', '.']).to_owned()
}

/// `2026-10-13` and `18.30.05` of an instant in local time.
fn stamp(ts_ms: i64, calendar: &dyn Calendar) -> (String, String) {
    const DAY_S: i64 = 86_400;
    let local_s = ts_ms
        .div_euclid(1000)
        .saturating_add(i64::from(calendar.utc_offset_s(ts_ms)));
    let date = civil_from_days(local_s.div_euclid(DAY_S));
    let in_day = local_s.rem_euclid(DAY_S);
    (
        format!("{:04}-{:02}-{:02}", date.year, date.month, date.day),
        format!(
            "{:02}.{:02}.{:02}",
            in_day / 3600,
            in_day % 3600 / 60,
            in_day % 60
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::{CivilDate, FixedOffset, days_from_civil};
    use proptest::prelude::*;

    /// 2026-10-13 `h:m:s` UTC, in ms.
    fn at(h: i64, m: i64, s: i64) -> i64 {
        let day = days_from_civil(CivilDate {
            year: 2026,
            month: 10,
            day: 13,
        });
        (day * 86_400 + h * 3600 + m * 60 + s) * 1000
    }

    fn facts<'a>(sender: &'a str, mime: Option<&'a str>) -> NameFacts<'a> {
        NameFacts {
            file_name: None,
            sender,
            sent_at_ms: at(18, 30, 5),
            position: 1,
            mime,
            sniffed: None,
        }
    }

    fn named<'a>(name: &'a str, sniffed: Option<&'a str>) -> NameFacts<'a> {
        NameFacts {
            file_name: Some(name),
            sniffed,
            ..facts("Ari", None)
        }
    }

    const UTC: FixedOffset = FixedOffset(0);

    /// N1: `<sender> <date> <time>.<ext>`, the time without a colon, an album's n-th with its
    /// number, the local time of the calendar.
    #[test]
    fn a_nameless_file_is_called_sender_date_time() {
        let jpg = Some("image/jpeg");
        assert_eq!(
            suggested_name(&facts("Ari", jpg), &UTC),
            "Ari 2026-10-13 18.30.05.jpg"
        );
        let second = NameFacts {
            position: 2,
            ..facts("Ari", jpg)
        };
        assert_eq!(
            suggested_name(&second, &UTC),
            "Ari 2026-10-13 18.30.05 2.jpg"
        );
        let third = NameFacts {
            position: 3,
            ..facts("Ari", jpg)
        };
        assert_eq!(
            suggested_name(&third, &UTC),
            "Ari 2026-10-13 18.30.05 3.jpg"
        );
        for position in [1, 2, 3, 12] {
            let n = NameFacts {
                position,
                ..facts("Ari", jpg)
            };
            assert!(
                !suggested_name(&n, &UTC).contains(':'),
                "no colon: FAT, SMB"
            );
        }
        // A video, no extension known, no sender.
        assert_eq!(
            suggested_name(&facts("Ari", Some("video/mp4")), &UTC),
            "Ari 2026-10-13 18.30.05.mp4"
        );
        assert_eq!(
            suggested_name(&facts("Ari", None), &UTC),
            "Ari 2026-10-13 18.30.05"
        );
        assert_eq!(
            suggested_name(&facts("", jpg), &UTC),
            "2026-10-13 18.30.05.jpg"
        );
        assert_eq!(
            suggested_name(&facts("  ", jpg), &UTC),
            "2026-10-13 18.30.05.jpg"
        );
        // The local time of the calendar, and the date that goes with it.
        assert_eq!(
            suggested_name(&facts("Ari", jpg), &FixedOffset(3 * 3600)),
            "Ari 2026-10-13 21.30.05.jpg"
        );
        assert_eq!(
            suggested_name(&facts("Ari", jpg), &FixedOffset(6 * 3600)),
            "Ari 2026-10-14 00.30.05.jpg"
        );
        assert_eq!(
            suggested_name(&facts("Ari", jpg), &FixedOffset(-19 * 3600)),
            "Ari 2026-10-12 23.30.05.jpg"
        );
        // A name that came to nothing is a nameless file.
        let junk = NameFacts {
            file_name: Some("..."),
            ..facts("Ari", jpg)
        };
        assert_eq!(suggested_name(&junk, &UTC), "Ari 2026-10-13 18.30.05.jpg");
    }

    /// A WhatsApp push name arrives as `~Ann`: the file does not start with a tilde (the shell
    /// expands `~Ann`). One mark is the label's; a second one is part of the name and is
    /// replaced like any leading tilde.
    #[test]
    fn a_push_name_loses_its_mark() {
        let jpg = Some("image/jpeg");
        assert_eq!(
            suggested_name(&facts("~Ann", jpg), &UTC),
            "Ann 2026-10-13 18.30.05.jpg"
        );
        assert_eq!(
            suggested_name(&facts("Ann", jpg), &UTC),
            "Ann 2026-10-13 18.30.05.jpg"
        );
        assert_eq!(
            suggested_name(&facts("~~Ann", jpg), &UTC),
            "_Ann 2026-10-13 18.30.05.jpg"
        );
        // A sender is cleaned like a name: no path, no option, no hiding mark.
        assert_eq!(
            suggested_name(&facts("-rf", jpg), &UTC),
            "_rf 2026-10-13 18.30.05.jpg"
        );
        assert_eq!(
            suggested_name(&facts("../x\u{202E}y", jpg), &UTC),
            "_xy 2026-10-13 18.30.05.jpg"
        );
    }

    #[test]
    fn the_extension_of_a_nameless_file_is_judged_by_the_content_first() {
        let name = |mime: Option<&str>, sniffed: Option<&str>| {
            let f = NameFacts {
                sniffed,
                ..facts("Ari", mime)
            };
            suggested_name(&f, &UTC)
        };
        let stem = "Ari 2026-10-13 18.30.05";
        // A PNG that calls itself a JPEG.
        assert_eq!(
            name(Some("image/jpeg"), Some("image/png")),
            format!("{stem}.png")
        );
        // A type that tells nothing, the content tells.
        assert_eq!(
            name(Some("application/octet-stream"), Some("video/mp4")),
            format!("{stem}.mp4")
        );
        assert_eq!(name(None, Some("image/webp")), format!("{stem}.webp"));
        // The stated type, when the content is no precise class.
        assert_eq!(name(Some("audio/aac"), None), format!("{stem}.aac"));
        assert_eq!(
            name(Some("image/jpeg"), Some("video/webm")),
            format!("{stem}.jpg"),
            "a container is not a judge"
        );
        assert_eq!(name(None, None), stem);
    }

    /// E2: the content corrects the extension only where it is precise, and never a container.
    #[test]
    fn extension_rule_e2() {
        let table: [(&str, Option<&str>, &str); 24] = [
            ("photo.pdf", Some("image/png"), "photo.pdf.png"),
            ("photo.jpg", Some("image/jpeg"), "photo.jpg"),
            ("photo.JPG", Some("image/jpeg"), "photo.JPG"),
            ("photo.jpeg", Some("image/jpeg"), "photo.jpeg"),
            ("PHOTO.PNG", Some("image/png"), "PHOTO.PNG"),
            ("IMG_0001", Some("image/png"), "IMG_0001.png"),
            ("notes.txt", Some("application/pdf"), "notes.txt.pdf"),
            ("archive.png", Some("application/zip"), "archive.png.zip"),
            ("track.mp3", Some("audio/flac"), "track.mp3.flac"),
            ("track.flac", Some("audio/mpeg"), "track.flac.mp3"),
            ("voice.wav", Some("audio/wav"), "voice.wav"),
            ("a.webm", Some("image/png"), "a.webm.png"),
            ("a.mp4", Some("image/gif"), "a.mp4.gif"),
            ("a.gif", Some("image/webp"), "a.gif.webp"),
            ("a.bmp", Some("image/jpeg"), "a.bmp.jpg"),
            // Containers: the sniffer calls every EBML file webm and every unknown ftyp brand
            // mp4, so the content is no judge of these.
            ("clip.mkv", Some("video/webm"), "clip.mkv"),
            ("clip.3gp", Some("video/mp4"), "clip.3gp"),
            ("clip.mov", Some("video/mp4"), "clip.mov"),
            ("song.m4a", Some("audio/mp4"), "song.m4a"),
            ("a", Some("video/mp4"), "a"),
            ("a.heic", Some("image/heic"), "a.heic"),
            // A ZIP is also a .docx, an .apk, an .epub: an extension the table does not know
            // is not called wrong.
            ("report.docx", Some("application/zip"), "report.docx"),
            ("a.pdf", None, "a.pdf"),
            ("a.pdf", Some("text/plain"), "a.pdf"),
        ];
        for (name, sniffed, want) in table {
            assert_eq!(
                suggested_name(&named(name, sniffed), &UTC),
                want,
                "{name} {sniffed:?}"
            );
        }
        // The added extension stays within the budget; the stem gives way.
        let long = format!("{}.pdf", "я".repeat(80));
        let fixed = suggested_name(&named(&long, Some("image/png")), &UTC);
        assert!(fixed.len() <= MAX_NAME_BYTES, "{}", fixed.len());
        assert!(fixed.ends_with(".png"), "{fixed}");
    }

    #[test]
    fn a_hostile_name_is_cleaned() {
        let table = [
            // Paths.
            ("../../.bashrc", "_.._.bashrc"),
            ("a/b", "a_b"),
            ("/etc/passwd", "_etc_passwd"),
            ("..\\..\\x.exe", "_.._x.exe"),
            ("C:\\Windows\\x", "C__Windows_x"),
            ("a:b*c?d\"e<f>g|h", "a_b_c_d_e_f_g_h"),
            // Dots and spaces.
            (".bashrc", "bashrc"),
            ("...hidden.txt", "hidden.txt"),
            ("photo.", "photo"),
            ("photo.jpg ", "photo.jpg"),
            ("  a   b  ", "a b"),
            ("a\t\n b", "a b"),
            // Options and the shell's tilde.
            ("-rf", "_rf"),
            ("--help.txt", "_-help.txt"),
            ("~root", "_root"),
            ("a-b~c", "a-b~c"),
            // Nothing left.
            ("", "Attachment"),
            ("   ", "Attachment"),
            ("...", "Attachment"),
            ("\u{202E}\u{200B}", "Attachment"),
            (".", "Attachment"),
            ("..", "Attachment"),
        ];
        for (raw, want) in table {
            assert_eq!(clean_name(raw), want, "{raw:?}");
        }
        // Controls vanish; NEL is white space, so a space.
        assert_eq!(clean_name("a\u{0}b\u{7}c\u{7f}d\u{85}e"), "abcd e");
        // A name that is only an extension keeps its stem.
        assert_eq!(clean_name(".gitignore"), "gitignore");
        assert_eq!(clean_name("a.b.c"), "a.b.c");
        // An "extension" with a space or beyond 16 bytes is part of the stem.
        assert_eq!(clean_name("a.b c"), "a.b c");
        let long_ext = format!("x.{}", "e".repeat(17));
        assert_eq!(clean_name(&long_ext), long_ext);
    }

    #[test]
    fn what_hides_in_a_link_hides_in_a_name_and_a_joiner_keeps_its_place() {
        // Bidi controls and zero-width characters go.
        assert_eq!(clean_name("evil\u{202E}gpj.exe"), "evilgpj.exe");
        assert_eq!(clean_name("a\u{200B}b.txt"), "ab.txt");
        assert_eq!(clean_name("\u{200F}name\u{200F}.txt"), "name.txt");
        assert_eq!(clean_name("a\u{2066}b\u{2069}.txt"), "ab.txt");
        assert_eq!(clean_name("a\u{FEFF}b.txt"), "ab.txt");
        assert_eq!(clean_name("a\u{FFFC}b.txt"), "ab.txt");
        // A joiner is judged by the neighbours that STAY: next to a character that goes it has
        // none (the proptest's first find: U+FFFC, U+200D, an inverted exclamation mark).
        assert_eq!(clean_name("\u{FFFC}\u{200D}\u{A1}"), "\u{A1}");
        assert_eq!(clean_name("\u{A1}\u{200D}\u{FFFC}a"), "\u{A1}a");
        // ... and by the ones that stay, when those make it fit.
        assert_eq!(
            clean_name("\u{A1}\u{200D}\u{FFFC}\u{A1}"),
            "\u{A1}\u{200D}\u{A1}"
        );
        // A joiner between ASCII letters, or at an edge, draws nothing: gone.
        assert_eq!(clean_name("ex\u{200D}ample.txt"), "example.txt");
        assert_eq!(clean_name("ex\u{200C}ample.txt"), "example.txt");
        assert_eq!(clean_name("x\u{200D}.txt"), "x.txt");
        // Persian keeps its zero-width non-joiner: it is the spelling.
        let persian = "\u{0645}\u{06CC}\u{200C}\u{062E}\u{0648}\u{0627}\u{0647}\u{0645}.pdf";
        assert_eq!(clean_name(persian), persian);
        // An Indic conjunct keeps the joiner after its virama.
        let indic = "\u{0915}\u{094D}\u{200D}\u{0937}.txt";
        assert_eq!(clean_name(indic), indic);
        // An emoji family keeps its joiners.
        let family = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}.png";
        assert_eq!(clean_name(family), family);
        // The same cleaning serves a sender's name.
        let moved = NameFacts {
            sender: "Ari\u{202E}\u{200B}",
            ..facts("", Some("image/png"))
        };
        assert_eq!(suggested_name(&moved, &UTC), "Ari 2026-10-13 18.30.05.png");
    }

    #[test]
    fn a_device_name_gets_an_underscore() {
        for (raw, want) in [
            ("CON", "_CON"),
            ("con.txt", "_con.txt"),
            ("Aux.tar.gz", "_Aux.tar.gz"),
            ("NUL", "_NUL"),
            ("prn.log", "_prn.log"),
            ("COM1", "_COM1"),
            ("COM9.dat", "_COM9.dat"),
            ("lpt3.txt", "_lpt3.txt"),
            ("LPT9", "_LPT9"),
            ("COM0.txt", "_COM0.txt"),
            ("lpt0", "_lpt0"),
            ("LPT\u{B9}", "_LPT\u{B9}"),
            ("com\u{B2}.txt", "_com\u{B2}.txt"),
            ("COM\u{B3}", "_COM\u{B3}"),
            ("con .txt", "_con.txt"),
            // Not devices.
            ("CONSOLE.txt", "CONSOLE.txt"),
            ("COM10", "COM10"),
            ("COM\u{B9}\u{B9}", "COM\u{B9}\u{B9}"),
            ("COM\u{2074}", "COM\u{2074}"),
            ("LPT", "LPT"),
            ("my con.txt", "my con.txt"),
            ("a.con", "a.con"),
        ] {
            assert_eq!(clean_name(raw), want, "{raw}");
        }
    }

    /// The cut falls right after a ZWNJ that was kept for the letter after it: the joiner goes
    /// too, so the stem does not end out of its place and the cleaner is idempotent there.
    #[test]
    fn a_cut_never_leaves_a_joiner_at_the_end_of_the_stem() {
        // Persian: 66 letters of two bytes, a ZWNJ of three (135 of the 136 bytes a stem with
        // ".pdf" may have), then letters the cut removes.
        let raw = format!(
            "{}\u{200C}{}.pdf",
            "\u{6CC}".repeat(66),
            "\u{6CC}".repeat(5)
        );
        // (The premise: a ZWNJ between two letters of the word is kept.)
        assert!(clean_name("\u{6CC}\u{200C}\u{6CC}").contains('\u{200C}'));
        // The cut itself: the joiner that ends the 136 bytes goes with the cut-off letters (the
        // cleaner would drop it in a second round, but a name that is cut and not cleaned again,
        // like the numbered one, must not end with it).
        let stem = format!("{}\u{200C}{}", "\u{6CC}".repeat(66), "\u{6CC}".repeat(5));
        assert_eq!(cut(&stem, 136), "\u{6CC}".repeat(66));
        assert_eq!(cut("a\u{200D}b", 4), "a");
        let name = clean_name(&raw);
        assert_eq!(name, format!("{}.pdf", "\u{6CC}".repeat(66)));
        assert_eq!(clean_name(&name), name);
        // The same through the numbered form (the next name of a batch): the cluster of the 66th
        // letter and its joiner no longer fits, so it goes whole.
        let numbered = with_number(&raw, 1);
        assert_eq!(numbered, format!("{} (1).pdf", "\u{6CC}".repeat(65)));
        assert_eq!(clean_name(&numbered), numbered);
        // A joiner inside the budget stays: the word is whole.
        let short = format!("{}\u{200C}{}.pdf", "\u{6CC}".repeat(3), "\u{6CC}".repeat(3));
        assert_eq!(clean_name(&short), short);
    }

    /// The cut can turn the tail of a name into an extension (the proptest's second find): the
    /// cleaner goes on until the name holds still.
    #[test]
    fn a_cleaned_name_cleans_to_itself() {
        // 136 letters, two dots, a word, a space, more: the space kept the tail from being an
        // extension; the cut at 140 bytes takes the rest away, and the first round leaves `..`.
        let raw = format!("{}..bb ccc", "a".repeat(136));
        let name = clean_name(&raw);
        assert!(name.len() <= MAX_NAME_BYTES);
        assert_eq!(clean_name(&name), name);
        assert!(!name.contains(".."), "{name}");
        // A sender called like a device is not one in the nameless name either.
        let device = NameFacts {
            sender: "COM1.x",
            ..facts("", Some("image/png"))
        };
        let nameless = suggested_name(&device, &UTC);
        assert_eq!(nameless, "_COM1.x 2026-10-13 18.30.05.png");
        assert_eq!(clean_name(&nameless), nameless);
    }

    #[test]
    fn the_budget_is_in_bytes_and_a_cluster_is_never_cut() {
        // Cyrillic: two bytes a letter; the extension whole.
        let name = clean_name(&format!("{}.pdf", "\u{44F}".repeat(300)));
        assert_eq!(name.len(), MAX_NAME_BYTES);
        assert!(name.ends_with(".pdf"));
        assert_eq!(name.chars().filter(|c| *c == '\u{44F}').count(), 68);
        // A name at the limit stays.
        let exact = format!("{}.txt", "a".repeat(MAX_NAME_BYTES - 4));
        assert_eq!(clean_name(&exact), exact);
        let over = format!("{}.txt", "a".repeat(MAX_NAME_BYTES - 3));
        assert_eq!(clean_name(&over), exact);
        // A letter and its accent are one cluster: three bytes; 45 of them fit.
        let accented = clean_name(&format!("{}.txt", "e\u{301}".repeat(100)));
        let stem = accented.strip_suffix(".txt").expect("extension kept");
        assert!(stem.graphemes(true).all(|g| g == "e\u{301}"), "{stem:?}");
        assert_eq!(stem.graphemes(true).count(), 45);
        // An emoji family is eighteen bytes and one cluster.
        let family = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}";
        let cut_family = clean_name(&family.repeat(20));
        assert!(cut_family.len() <= MAX_NAME_BYTES);
        assert!(cut_family.graphemes(true).all(|g| g == family));
        assert_eq!(cut_family.graphemes(true).count(), 7);
        // One cluster above the budget leaves nothing: the generic word.
        let huge = format!("e{}", "\u{301}".repeat(200));
        assert_eq!(clean_name(&huge), "Attachment");
        assert_eq!(clean_name(&format!("{huge}.txt")), "Attachment.txt");
        // A cut that ends on a space or a dot does not leave it.
        let ragged = format!("{} .b{}", "a".repeat(MAX_NAME_BYTES - 2), "c".repeat(30));
        assert_eq!(clean_name(&ragged), "a".repeat(MAX_NAME_BYTES - 2));
        // A sender's long name gives way, the date, the time and the extension do not.
        let sender = "\u{44F}".repeat(200);
        let nameless = suggested_name(&facts(&sender, Some("image/jpeg")), &UTC);
        assert!(nameless.len() <= MAX_NAME_BYTES, "{}", nameless.len());
        assert!(nameless.ends_with(" 2026-10-13 18.30.05.jpg"), "{nameless}");
        let album = NameFacts {
            position: 4,
            ..facts(&sender, Some("image/jpeg"))
        };
        let nameless = suggested_name(&album, &UTC);
        assert!(nameless.len() <= MAX_NAME_BYTES);
        assert!(
            nameless.ends_with(" 2026-10-13 18.30.05 4.jpg"),
            "{nameless}"
        );
        // A sender that is one huge cluster is left out.
        assert_eq!(
            suggested_name(&facts(&huge, Some("image/jpeg")), &UTC),
            "2026-10-13 18.30.05.jpg"
        );
    }

    #[test]
    fn a_clash_gets_a_number_inside_the_budget() {
        assert_eq!(with_number("report.pdf", 1), "report (1).pdf");
        assert_eq!(with_number("report.pdf", 12), "report (12).pdf");
        assert_eq!(with_number("notes", 2), "notes (2)");
        assert_eq!(with_number("a.tar.gz", 3), "a.tar (3).gz");
        let long = with_number(&format!("{}.pdf", "a".repeat(200)), 7);
        assert_eq!(long.len(), MAX_NAME_BYTES);
        assert!(long.ends_with(" (7).pdf"), "{long}");
        // The number is part of the name that is cleaned too.
        assert_eq!(with_number("../x", 1), "_x (1)");
    }

    fn piece() -> impl Strategy<Value = char> {
        prop_oneof![
            3 => any::<char>(),
            5 => proptest::sample::select(vec![
                '/', '\\', '.', '-', '~', ' ', '\0', '\u{202E}', '\u{200D}', '\u{200C}',
                '\u{200B}', '\u{094D}', ':', 'a', 'C', 'O', 'N', '\u{301}', '\n', '\u{85}',
                '\u{1F468}', '\u{FFFC}', 'x', 'я',
            ]),
        ]
    }

    proptest! {
        /// Whatever the sender wrote, the name is one path component that no tool takes for an
        /// option, a hidden file or a path, within the budget.
        #[test]
        fn any_name_is_safe(raw in proptest::collection::vec(piece(), 0..120)) {
            let raw: String = raw.into_iter().collect();
            let name = clean_name(&raw);
            // The cleaner is idempotent: a name the dialog was offered, or one the user left
            // as it was, is saved under the name he saw.
            prop_assert_eq!(clean_name(&name), name.clone());
            prop_assert!(!name.is_empty());
            prop_assert!(name.len() <= MAX_NAME_BYTES, "{}", name.len());
            prop_assert!(!name.contains(['/', '\\', '\0', ':', '*', '?', '"', '<', '>', '|']));
            prop_assert!(!name.chars().any(char::is_control));
            prop_assert!(!name.starts_with(['.', '-', '~', ' ']), "{name:?}");
            prop_assert!(!name.ends_with(['.', ' ']), "{name:?}");
            prop_assert!(name != "." && name != "..");
            let hides = name.chars().any(|c| {
                matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{200B}' | '\u{FFFC}')
            });
            prop_assert!(!hides);
            let numbered = with_number(&raw, 3);
            prop_assert!(numbered.len() <= MAX_NAME_BYTES);
            prop_assert!(!numbered.contains('/'));
            // The nameless form keeps its date, time and extension whole.
            let f = NameFacts {
                sender: &raw,
                ..facts("", Some("image/png"))
            };
            let nameless = suggested_name(&f, &UTC);
            prop_assert!(nameless.len() <= MAX_NAME_BYTES);
            prop_assert_eq!(clean_name(&nameless), nameless.clone());
            prop_assert!(nameless.ends_with("2026-10-13 18.30.05.png"), "{nameless:?}");
            prop_assert!(!nameless.starts_with(['.', '-', '~', ' ']), "{nameless:?}");
            prop_assert!(!nameless.contains(['/', '\0', ':']));
        }
    }
}
