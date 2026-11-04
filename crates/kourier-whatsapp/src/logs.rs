//! What upstream's log lines may say (W2, chunk C6a; T-10; critic H4, lead's decision L7).
//!
//! whatsapp-rust and the crates under it log through `log`, with their own targets ("Client/AppState",
//! "Bot/PairCode", 42 of them), and some of their lines carry what Kourier never writes down: a whole
//! stanza (`Got {reason:?} connect failure, logging out: {node}` holds an account lock's one-time
//! `appeal_token`; an IQ holds the sender's push NAME in `notify`), a phone number, a JID, a LID, the pair
//! code itself, a key's hex. This module is the pure part of the answer; the wiring into the app's log (a
//! filter on each record's module path in front of the writer, W2b) only calls it:
//!
//! - [`is_upstream`] says whether a record comes from those crates, by its MODULE PATH. The target cannot
//!   say it: the custom targets are free words, and the formatted line does not carry the module.
//! - [`keeps`] is the level rule for those records: nothing below WARN is written, whatever `KOURIER_LOG`
//!   says (their INFO lines print numbers and the pair code).
//! - [`redact_upstream`] is what a record that is kept goes through, on its MESSAGE only (never on a
//!   formatted line, which would also mangle Kourier's own counts and times): a stanza dump becomes the
//!   names of its tags, every quoted string and every attribute value is hidden, runs that look like a
//!   token or a key are hidden, so are JID users, the pair code and number after the lines that print
//!   them, and every run of six or more digits (a phone number, a LID, a ProtocolAddress `<digits>.<n>`).
//!   Two guards for pin bumps (review F3): a bracketed list of eight or more small integers (a key or
//!   hash printed with `{:?}` as bytes) becomes `[…]`, and what follows a word that says "name" (`name`,
//!   `push_name`, `username`, ...) is hidden to the end of the message, because a name printed
//!   unquoted has no shape to find it by. Neither was a leak at the pinned revision; they are for the
//!   next one.
//!
//! PIN BUMPS (the checklist is also in the root `Cargo.toml`'s comment on the pin): re-read upstream's new
//! `warn!` and `error!` format strings against these rules and add what is new to the corpus in
//! `logs/tests.rs`, and keep upstream's `tracing` feature OFF (its span fields carry the account's LID and
//! number, which a redaction of the message cannot reach).
//!
//! The rules prefer to hide too much: an upstream WARN line is a hint that something happened, not a
//! place to read the data. The result of [`redact_upstream`] is stable (applied twice it changes
//! nothing). Nothing here logs.

use tracing::Level;

/// The crates whose records are upstream's, by the first segment of the module path (a `-` in a crate
/// name is `_` there). Besides the crates of the whatsapp-rust workspace (checked against `cargo
/// metadata` by a test, so a crate added by a pin bump cannot slip by) it holds the HTTP, TLS and
/// WebSocket crates under them: `ureq` prints the path and query of every request at TRACE and the
/// wire rows of `ureq_proto` (critic H4), and media uploads carry `auth=` tokens in the URL from W6 on.
/// The workspace's transport and HTTP-client crates are the family [`UPSTREAM_FAMILY`], not names here.
pub const UPSTREAM_MODULES: &[&str] = &[
    "whatsapp_rust",
    "wacore",
    "wacore_appstate",
    "wacore_binary",
    "wacore_derive",
    "wacore_libsignal",
    "wacore_noise",
    "waproto",
    "buffa",
    "ureq",
    "ureq_proto",
    "rustls",
    "tokio_rustls",
    "tokio_websockets",
    "webpki",
];

/// The crates of the whatsapp-rust workspace that are named `whatsapp_rust_<what they are>`.
pub const UPSTREAM_FAMILY: &str = "whatsapp_rust_";

/// What a hidden piece is written as.
const HIDDEN: char = '…';

/// Whether a record whose `log` module path is `module_path` comes from upstream. Only the first segment
/// counts, and it must be a whole crate name (`whatsapp_rustic::x` is not upstream's). A custom target
/// such as "Client/AppState" is not a module path: the caller passes `log.module_path`.
pub fn is_upstream(module_path: &str) -> bool {
    let root = module_path.split("::").next().unwrap_or("");
    UPSTREAM_MODULES.contains(&root) || root.starts_with(UPSTREAM_FAMILY)
}

/// Whether a record may be written at all: upstream's only from WARN up (tracing orders levels by
/// verbosity, so ERROR and WARN are the two that compare at most WARN), everyone else's as the filters
/// of the app say.
pub fn keeps(module_path: &str, level: Level) -> bool {
    level <= Level::WARN || !is_upstream(module_path)
}

/// The message of an upstream record that passed [`keeps`], with nothing in it that names anybody. The
/// rules are applied in this order: stanza dumps, quoted strings, byte lists, what follows a word that
/// says "name", the pair code and number after the lines that print them, JID users, long token-like
/// runs, digit runs.
pub fn redact_upstream(message: &str) -> String {
    let text = cut_stanzas(message);
    let text = hide_quoted(&text);
    let text = hide_byte_lists(&text);
    let text = hide_after_names(&text);
    let text = hide_after_labels(&text);
    let text = hide_jid_users(&text);
    let text = hide_long_runs(&text);
    hide_digit_runs(&text)
}

/// A run of this many digits or more is hidden: a phone number, a LID, the user of a ProtocolAddress.
const DIGITS_HIDDEN_FROM: usize = 6;

/// A run of this many token characters or more may be a token or a key.
const TOKEN_MIN: usize = 16;

/// ... and one this long is hidden whatever it is made of (an identifier is not that long).
const TOKEN_ALWAYS: usize = 32;

/// A bracketed list of this many small integers or more is a key or a hash printed as bytes.
const BYTE_LIST_MIN: usize = 8;

/// The lines that print a secret as their next word. The text is what the line says before it (matched
/// without regard to case); the word after it is hidden, whatever it is made of: the pair code may be
/// letters alone, which no digit rule sees. They are upstream's own INFO lines ("Pair code generated:",
/// "... waiting for phone confirmation. Code:", "Starting pair code authentication for phone:").
const LABELS: &[&str] = &["pair code generated:", "confirmation. code:", "for phone:"];

/// Characters of a stanza's tag name.
fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | ':' | '-' | '.')
}

/// Characters of the user part of a JID (or an address): digits, letters, the device and agent marks
/// and a `+`.
fn is_user_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '+' | '-')
}

/// Characters of a token or key as base64, base64url or hex write it (the `=` padding is looked for
/// after a run, see [`hide_long_runs`]).
fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '_' | '-')
}

/// Where the first stanza dump of `chars` begins: a `<`, a tag name, and then a space (attributes follow),
/// `/>` (an empty tag) or `>` that a `</name>` later in the text closes. A generic type such as
/// `Option<Jid>` is none of these and stays.
fn stanza_start(chars: &[char]) -> Option<usize> {
    for (at, &c) in chars.iter().enumerate() {
        if c != '<'
            || !chars
                .get(at + 1)
                .is_some_and(|n| n.is_ascii_alphabetic() || *n == '_')
        {
            continue;
        }
        let name_end = (at + 1..chars.len())
            .find(|&i| !is_name_char(chars[i]))
            .unwrap_or(chars.len());
        let rest = &chars[name_end..];
        let opens = match rest.first() {
            Some(' ' | '\t' | '\n') => true,
            Some('/') => rest.get(1) == Some(&'>'),
            Some('>') => {
                let closer: Vec<char> = "</"
                    .chars()
                    .chain(chars[at + 1..name_end].iter().copied())
                    .chain(['>'])
                    .collect();
                rest.windows(closer.len()).any(|w| w == closer.as_slice())
            }
            _ => false,
        };
        if opens {
            return Some(at);
        }
    }
    None
}

/// Replaces everything from the first stanza dump on with the names of its tags, opening tags only and
/// in order (`<failure><locked>`): attribute values, text and the closing tags go. A quote inside a tag
/// hides a `>` from the scan.
fn cut_stanzas(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let Some(start) = stanza_start(&chars) else {
        return text.to_owned();
    };
    let mut out: String = chars[..start].iter().collect();
    let mut i = start;
    while i < chars.len() {
        if chars[i] != '<' {
            i += 1;
            continue;
        }
        match chars.get(i + 1) {
            Some('/') => {
                // A closing tag: its name was written by the opening one.
                while i < chars.len() && chars[i] != '>' {
                    i += 1;
                }
            }
            Some(n) if n.is_ascii_alphabetic() || *n == '_' => {
                let name_end = (i + 1..chars.len())
                    .find(|&j| !is_name_char(chars[j]))
                    .unwrap_or(chars.len());
                out.push('<');
                out.extend(&chars[i + 1..name_end]);
                out.push('>');
                // The rest of the tag: attributes, up to the `>` that no quote hides.
                i = name_end;
                let mut quoted = false;
                while i < chars.len() {
                    match chars[i] {
                        '"' => quoted = !quoted,
                        '>' if !quoted => break,
                        _ => {}
                    }
                    i += 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    out
}

/// Replaces the inside of every double-quoted string with the mark (`reason="403"` becomes
/// `reason="…"`): names, tokens and server texts come quoted. A backslash escapes the next character; a
/// string that is not closed runs to the end.
fn hide_quoted(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        out.push(c);
        if c != '"' {
            continue;
        }
        let mut closed = false;
        while let Some(inner) = chars.next() {
            match inner {
                '\\' => {
                    chars.next();
                }
                '"' => {
                    closed = true;
                    break;
                }
                _ => {}
            }
        }
        out.push(HIDDEN);
        if closed {
            out.push('"');
        }
    }
    out
}

/// Where the small integer at `chars[at..]` ends: one to three decimal digits, or `0x` and one or two
/// hex digits, not followed by a letter or a digit (`None` when there is none).
fn small_int_end(chars: &[char], at: usize) -> Option<usize> {
    let hex = chars.get(at) == Some(&'0') && matches!(chars.get(at + 1), Some('x' | 'X'));
    let (from, max, digit): (usize, usize, fn(&char) -> bool) = if hex {
        (at + 2, 2, |c| c.is_ascii_hexdigit())
    } else {
        (at, 3, |c| c.is_ascii_digit())
    };
    let len = chars[from.min(chars.len())..]
        .iter()
        .take_while(|c| digit(c))
        .count();
    let end = from + len;
    let ok = (1..=max).contains(&len) && !chars.get(end).is_some_and(|c| c.is_ascii_alphanumeric());
    ok.then_some(end)
}

/// Replaces every bracketed list of [`BYTE_LIST_MIN`] or more small integers (`[5, 161, 178, ...]`, as
/// `{:?}` prints a byte slice, also cut short with an ellipsis or not closed) with `[…]`.
fn hide_byte_lists(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    while at < chars.len() {
        if chars[at] != '[' {
            out.push(chars[at]);
            at += 1;
            continue;
        }
        let mut i = at + 1;
        let mut items = 0;
        loop {
            while chars.get(i).is_some_and(|c| c.is_whitespace()) {
                i += 1;
            }
            let Some(end) = small_int_end(&chars, i) else {
                break;
            };
            items += 1;
            i = end;
            while chars.get(i).is_some_and(|c| c.is_whitespace()) {
                i += 1;
            }
            if chars.get(i) == Some(&',') {
                i += 1;
            } else {
                break;
            }
        }
        if items < BYTE_LIST_MIN {
            out.push('[');
            at += 1;
            continue;
        }
        // The rest of the list: a trailing comma or ellipsis, then the closing bracket.
        while chars
            .get(i)
            .is_some_and(|c| c.is_whitespace() || matches!(c, ',' | '.'))
        {
            i += 1;
        }
        if chars.get(i) == Some(&']') {
            i += 1;
        }
        out.extend(['[', HIDDEN, ']']);
        at = i;
    }
    out
}

/// Whether a lowercase word says "name": `name`, `names`, a `..._name` or `..._names`, or one of the
/// compounds upstream writes together.
fn says_name(word: &str) -> bool {
    ["name", "names"].contains(&word)
        || ["_name", "_names", "pushname", "username", "displayname"]
            .iter()
            .any(|tail| word.ends_with(tail))
}

/// Hides what follows a word that says "name" (`name`, `push_name`, `username`, `names`, ...) when a
/// space, `:` or `=` comes right after it and something plain follows that: the rest of the message,
/// whose end the rule cannot know. A name is printed unquoted by some format strings and then has no
/// shape of its own. A quoted value (already hidden by [`hide_quoted`]), an end of message and a
/// message already cut at the word are left alone, so a second application changes nothing.
fn hide_after_names(text: &str) -> String {
    let word_char = |c: char| c.is_ascii_alphabetic() || c == '_';
    let separator = |c: char| c.is_whitespace() || matches!(c, ':' | '=');
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    while at < chars.len() {
        if !word_char(chars[at]) {
            out.push(chars[at]);
            at += 1;
            continue;
        }
        let start = at;
        while at < chars.len() && word_char(chars[at]) {
            at += 1;
        }
        out.extend(&chars[start..at]);
        let word: String = chars[start..at]
            .iter()
            .collect::<String>()
            .to_ascii_lowercase();
        if !says_name(&word) || !chars.get(at).is_some_and(|c| separator(*c)) {
            continue;
        }
        let mut rest = at;
        while chars.get(rest).is_some_and(|c| separator(*c)) {
            rest += 1;
        }
        if matches!(chars.get(rest).copied(), None | Some('"' | HIDDEN)) {
            continue;
        }
        out.extend(&chars[at..rest]);
        out.push(HIDDEN);
        return out;
    }
    out
}

/// Hides the word after each of the [`LABELS`].
fn hide_after_labels(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    while at < text.len() {
        // The labels are ASCII; `get` is `None` where the cut would fall inside a character.
        let label = LABELS.iter().find(|label| {
            text.get(at..at + label.len())
                .is_some_and(|head| head.eq_ignore_ascii_case(label))
        });
        let Some(label) = label else {
            let c = text[at..].chars().next().unwrap_or(' ');
            out.push(c);
            at += c.len_utf8();
            continue;
        };
        out.push_str(&text[at..at + label.len()]);
        at += label.len();
        let rest = &text[at..];
        let spaces = rest.len() - rest.trim_start().len();
        out.push_str(&rest[..spaces]);
        let word_len = rest[spaces..]
            .find(char::is_whitespace)
            .unwrap_or(rest.len() - spaces);
        if word_len > 0 {
            out.push(HIDDEN);
        }
        at += spaces + word_len;
    }
    out
}

/// Hides the user of every `<user>@<server>`: a JID of any server (`s.whatsapp.net`, `lid`, `g.us`,
/// `newsletter`, ...) with its device and agent marks, and an e-mail address as well. The server stays.
fn hide_jid_users(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out: Vec<char> = Vec::with_capacity(chars.len());
    for (at, &c) in chars.iter().enumerate() {
        let server_follows = chars.get(at + 1).is_some_and(|n| n.is_ascii_alphanumeric());
        if c == '@' && server_follows {
            let kept = out
                .iter()
                .rposition(|u| !is_user_char(*u))
                .map_or(0, |p| p + 1);
            if kept < out.len() {
                out.truncate(kept);
                out.push(HIDDEN);
            }
        }
        out.push(c);
    }
    out.into_iter().collect()
}

/// Hides every run of token characters that may be a token or a key: [`TOKEN_MIN`] or more with a digit,
/// a `+` or `/` in it or base64's `=` padding after it, or [`TOKEN_ALWAYS`] or more of anything. A
/// long identifier of letters and underscores stays, so a function's name is still readable; an `=`
/// that is not padding (`user=...`) ends a run.
fn hide_long_runs(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    while at < chars.len() {
        if !is_token_char(chars[at]) {
            out.push(chars[at]);
            at += 1;
            continue;
        }
        let start = at;
        while at < chars.len() && is_token_char(chars[at]) {
            at += 1;
        }
        let run = &chars[start..at];
        let padding = chars[at..]
            .iter()
            .take(2)
            .take_while(|c| **c == '=')
            .count();
        let looks_random = padding > 0
            || run
                .iter()
                .any(|c| c.is_ascii_digit() || matches!(c, '+' | '/'));
        let len = run.len() + padding;
        if len >= TOKEN_ALWAYS || (len >= TOKEN_MIN && looks_random) {
            out.push(HIDDEN);
            at += padding;
        } else {
            out.extend(run);
        }
    }
    out
}

/// Hides every run of [`DIGITS_HIDDEN_FROM`] digits or more.
fn hide_digit_runs(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut digits = String::new();
    let flush = |digits: &mut String, out: &mut String| {
        if digits.len() >= DIGITS_HIDDEN_FROM {
            out.push(HIDDEN);
        } else {
            out.push_str(digits);
        }
        digits.clear();
    };
    for c in text.chars() {
        if c.is_ascii_digit() {
            digits.push(c);
        } else {
            flush(&mut digits, &mut out);
            out.push(c);
        }
    }
    flush(&mut digits, &mut out);
    out
}

#[cfg(test)]
mod tests;
