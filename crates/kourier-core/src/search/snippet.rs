//! The snippet of a search hit (G1s): the bit of a message that shows under its chat.
//!
//! The index is contentless, so FTS5 cannot cut a snippet: [`snippet`] cuts it in Rust from
//! the message's [`Body`] as it is NOW, with the same tokens the index was written with
//! ([`normalize`]). It is also the last check of a hit:
//!
//! - **stale**: a row whose text no longer holds every term (an edit between the flush and
//!   the query, a body rewritten under the index) gives `None`, and the query drops it;
//! - **side by side**: the index has no positions (`detail=column`), so terms typed with
//!   nothing between them ("日本語", [`QueryTerm::joined`]) must stand side by side in one
//!   piece of the text, else `None` (the false hit of three ideographs scattered in a row).
//!
//! What a snippet may show (critic H1): `text` is the raw cut, so `ranges` carries the
//! body's spoiler and mention ranges clipped to the cut, in `text`'s offsets, and the
//! viewmodel renders them by the rule of the chat list's previews (`■■■`, `@Name`). A match
//! that touches a spoiler gets NO mark (a bold word in a hidden stretch would tell where the
//! word is); the row stays a hit, as in Signal Desktop's results. The invisible and bidi
//! controls stay in `text`: the viewmodel cleans them when it renders, by the rule that keeps
//! a Persian or Hebrew word's joiners.
//!
//! `Debug` of [`Snippet`] shows counts, never a text (architecture §9.1).

use std::fmt;

use kourier_model::{Body, RangeKind, Text, TextRange};
use unicode_segmentation::UnicodeSegmentation;

use super::normalize::{Piece, QueryTerm, Token, normalize, searched_texts};

/// Characters of context kept before the first match, at most (F1, R3): the hit row is one
/// line of about 30 characters after the sender, and a match that stands further in is elided
/// away (the row cannot show why it is a hit). 16 is Signal Desktop's and Telegram's order.
pub(crate) const BEFORE_CHARS: usize = 16;
/// Characters of context when no word starts within [`BEFORE_CHARS`] of the match (a long word,
/// an unbroken run): exactly this many, after the ellipsis.
pub(crate) const BEFORE_CHARS_CUT: usize = 12;
/// Characters of the source a snippet shows, at most (the ellipses not counted).
pub(crate) const SNIPPET_CHARS: usize = 120;
/// Stands where the cut left text out, at either end.
pub(crate) const ELLIPSIS: char = '\u{2026}';

/// The piece of a message that shows a search hit ([`crate::facade::SearchHit`]).
#[derive(Clone, PartialEq, Eq)]
pub struct Snippet {
    /// The cut, one line: white space and control characters (line breaks) became one space,
    /// an ellipsis (`…`) where text was left out. Raw otherwise: a spoiler's text is in it, to be
    /// hidden by `ranges`, and a mention is its U+FFFC.
    pub text: String,
    /// What matched, UTF-8 byte ranges of `text`: sorted, apart, never touching a spoiler of
    /// `ranges`. A prefix term marks the whole word it matched.
    pub marks: Vec<Mark>,
    /// The body's [`RangeKind::Spoiler`] ranges (clipped to the cut, overlapping ones joined)
    /// and its shown mentions ([`Text::shown_mentions`]) inside the cut, in `text`'s offsets,
    /// sorted by start. Empty for a piece with no formatting (a file name, a contact's name).
    pub ranges: Vec<TextRange>,
    /// The attachment whose file name the snippet shows: its place among the message's
    /// attachments (`attachments.idx`). `None` when the snippet is a text or a caption.
    pub file: Option<u32>,
}

/// One match in [`Snippet::text`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mark {
    /// UTF-8 byte offset into the snippet's text.
    pub start: u32,
    /// Length in UTF-8 bytes.
    pub len: u32,
}

impl fmt::Debug for Snippet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let spoilers = self
            .ranges
            .iter()
            .filter(|r| r.kind == RangeKind::Spoiler)
            .count();
        write!(
            f,
            "Snippet {{ bytes: {}, marks: {}, spoilers: {}, mentions: {}, file: {:?} }}",
            self.text.len(),
            self.marks.len(),
            spoilers,
            self.ranges.len() - spoilers,
            self.file
        )
    }
}

/// One place where a group of terms stands in a piece: byte range in the piece's text.
#[derive(Clone, Copy)]
struct Occurrence {
    group: usize,
    start: usize,
    end: usize,
}

/// The snippet of `body` for the query `terms` ([`super::normalize::query_terms`]), or
/// `None` when the body does not hold every term (stale, or joined terms not side by side,
/// or no terms at all, or a kind that is never searched).
///
/// The piece that shows it is the one that holds the most of the query's groups (the first
/// such piece in the message's order); the cut starts at most [`BEFORE_CHARS`] characters
/// before the first match that touches no spoiler (or the first match, if each one does),
/// at a word start when one lies within them, else exactly [`BEFORE_CHARS_CUT`] characters
/// before it (logical order, so the same for right-to-left text), and shows at most
/// [`SNIPPET_CHARS`] characters, the end on a word border, as long as the match fits.
pub(crate) fn snippet(body: &Body, terms: &[QueryTerm]) -> Option<Snippet> {
    let groups = groups(terms);
    if groups.is_empty() {
        return None;
    }
    let mut held = vec![false; groups.len()];
    let mut best: Option<(usize, Piece<'_>, Vec<Occurrence>)> = None;
    for piece in searched_texts(body) {
        let tokens = normalize(piece.text);
        let found = occurrences(&tokens, &groups);
        if found.is_empty() {
            continue;
        }
        let mut here = vec![false; groups.len()];
        for o in &found {
            here[o.group] = true;
            held[o.group] = true;
        }
        let score = here.iter().filter(|h| **h).count();
        if best.as_ref().is_none_or(|(s, _, _)| score > *s) {
            best = Some((score, piece, found));
        }
    }
    if !held.iter().all(|h| *h) {
        return None;
    }
    let (_, piece, found) = best?;
    Some(cut(piece, &found))
}

/// The most matches of a pattern one snippet marks.
const PATTERN_MARKS: usize = 64;

/// The snippet of `body` for a PATTERN search (R5): the first searched piece in which `re`
/// has a non-empty match, cut around the first match that touches no spoiler (as [`snippet`]),
/// every match of the piece marked. `None` when no piece has one (an empty match is no hit).
pub(crate) fn pattern_snippet(body: &Body, re: &regex::Regex) -> Option<Snippet> {
    for piece in searched_texts(body) {
        let found: Vec<Occurrence> = re
            .find_iter(piece.text)
            .filter(|m| !m.is_empty())
            .take(PATTERN_MARKS)
            .map(|m| Occurrence {
                group: 0,
                start: m.start(),
                end: m.end(),
            })
            .collect();
        if !found.is_empty() {
            return Some(cut(piece, &found));
        }
    }
    None
}

/// The query's terms in groups that must stand side by side: a new group starts at each
/// term that is not [`QueryTerm::joined`] (and at the first one).
fn groups(terms: &[QueryTerm]) -> Vec<&[QueryTerm]> {
    let mut out = Vec::new();
    let mut start = 0;
    for (i, term) in terms.iter().enumerate().skip(1) {
        if !term.joined {
            out.push(&terms[start..i]);
            start = i;
        }
    }
    if start < terms.len() {
        out.push(&terms[start..]);
    }
    out
}

/// Every place where a group stands in `tokens`: each term matches its token, in order, and
/// each token's source ends where the next one's starts.
fn occurrences(tokens: &[Token], groups: &[&[QueryTerm]]) -> Vec<Occurrence> {
    let mut out = Vec::new();
    for (group, terms) in groups.iter().enumerate() {
        let n = terms.len();
        if n == 0 {
            continue;
        }
        for run in tokens.windows(n) {
            let side_by_side = run.windows(2).all(|w| w[0].end == w[1].start);
            if side_by_side && terms.iter().zip(run).all(|(t, tok)| t.matches(&tok.text)) {
                out.push(Occurrence {
                    group,
                    start: run[0].start,
                    end: run[n - 1].end,
                });
            }
        }
    }
    out.sort_by_key(|o| (o.start, o.end));
    out
}

/// What the snippet treats as white space: what the chat list's one-line rule does
/// (`single_line` in the viewmodel), line breaks among it.
fn is_blank(c: char) -> bool {
    c.is_whitespace() || c.is_control()
}

/// The spoilers of `formatted`, byte ranges of its body: an end past the body is the body's
/// end, a range that does not fall on character borders is WIDENED to them (it hides more,
/// never less), overlapping or touching ones are joined, sorted.
fn spoilers(formatted: Option<&Text>) -> Vec<(usize, usize)> {
    let Some(text) = formatted else {
        return Vec::new();
    };
    let body = text.body.as_str();
    let mut out: Vec<(usize, usize)> = text
        .ranges
        .iter()
        .filter(|r| r.kind == RangeKind::Spoiler)
        .filter_map(|r| {
            let mut start = (r.start as usize).min(body.len());
            let mut end = r.end().map_or(body.len(), |e| (e as usize).min(body.len()));
            while !body.is_char_boundary(start) {
                start -= 1;
            }
            while !body.is_char_boundary(end) {
                end += 1;
            }
            (start < end).then_some((start, end))
        })
        .collect();
    out.sort_unstable();
    let mut joined: Vec<(usize, usize)> = Vec::with_capacity(out.len());
    for (start, end) in out {
        match joined.last_mut() {
            Some(last) if start <= last.1 => last.1 = last.1.max(end),
            _ => joined.push((start, end)),
        }
    }
    joined
}

/// Where the cut of `s` around the match `start..end` begins and ends (byte offsets).
fn window(s: &str, start: usize, end: usize) -> (usize, usize) {
    let bounds: Vec<usize> = s
        .split_word_bound_indices()
        .map(|(i, _)| i)
        .chain([s.len()])
        .collect();
    // The earliest word border at most BEFORE_CHARS characters before the match; when none
    // lies there, exactly BEFORE_CHARS_CUT characters before it.
    let limit = s[..start]
        .char_indices()
        .rev()
        .take(BEFORE_CHARS)
        .last()
        .map_or(start, |(i, _)| i);
    let border = bounds[bounds.partition_point(|&b| b < limit)];
    let mut from = if border <= start {
        border
    } else {
        s[..start]
            .char_indices()
            .rev()
            .take(BEFORE_CHARS_CUT)
            .last()
            .map_or(start, |(i, _)| i)
    };
    while from < start {
        match s[from..].chars().next() {
            Some(c) if is_blank(c) => from += c.len_utf8(),
            _ => break,
        }
    }
    let to_from = |from: usize| {
        let limit = s[from..]
            .char_indices()
            .nth(SNIPPET_CHARS)
            .map_or(s.len(), |(i, _)| from + i);
        let border = bounds[bounds.partition_point(|&b| b <= limit) - 1];
        (border, limit)
    };
    let (mut to, _) = to_from(from);
    if to < end {
        // The match does not fit after its context: drop the context, and cut inside the
        // match only when it alone is longer than the snippet.
        from = start;
        let (border, limit) = to_from(from);
        to = if border >= end { border } else { limit };
    }
    (from, to)
}

/// The snippet of `piece` cut around one of `found` (not empty).
fn cut(piece: Piece<'_>, found: &[Occurrence]) -> Snippet {
    let s = piece.text;
    let spoilers = spoilers(piece.formatted);
    // A match that overlaps OR touches a spoiler (ends where it begins, begins where it ends)
    // gets no mark, as the doc of [`Snippet::marks`] says.
    let hidden = |start: usize, end: usize| spoilers.iter().any(|&(a, b)| start <= b && a <= end);
    let shown = found
        .iter()
        .find(|o| !hidden(o.start, o.end))
        .or(found.first())
        .copied()
        .unwrap_or(Occurrence {
            group: 0,
            start: 0,
            end: 0,
        });
    let (from, to) = window(s, shown.start, shown.end);

    // The text, and where each character of the source landed in it.
    let mut text = String::with_capacity(to - from + 8);
    if s[..from].chars().any(|c| !is_blank(c)) {
        text.push(ELLIPSIS);
    }
    let mut landed: Vec<(usize, usize)> = Vec::with_capacity(to - from + 1);
    let mut content = false;
    let mut blank = false;
    for (i, c) in s[from..to].char_indices() {
        if is_blank(c) {
            blank = content;
            landed.push((from + i, text.len()));
            continue;
        }
        if blank {
            text.push(' ');
            blank = false;
        }
        landed.push((from + i, text.len()));
        text.push(c);
        content = true;
    }
    landed.push((to, text.len()));
    let at = |source: usize| -> u32 {
        let source = source.clamp(from, to);
        let i = landed.partition_point(|&(src, _)| src < source);
        let dst = landed.get(i).map_or(text.len(), |&(_, dst)| dst);
        u32::try_from(dst).unwrap_or(u32::MAX)
    };
    let span = |start: usize, end: usize| -> Option<(u32, u32)> {
        let (start, end) = (start.max(from), end.min(to));
        if start >= end {
            return None;
        }
        let (a, b) = (at(start), at(end));
        (a < b).then_some((a, b - a))
    };

    // The spoilers in the text's own offsets: blanks the cut joined can bring a spoiler that
    // began a blank after a match up to it (both land on the one space), so the touch rule is
    // asked again where the marks live.
    let spans: Vec<(u32, u32)> = spoilers.iter().filter_map(|&(a, b)| span(a, b)).collect();
    let mut marks: Vec<Mark> = found
        .iter()
        .filter(|o| !hidden(o.start, o.end))
        .filter_map(|o| span(o.start, o.end))
        .filter(|&(start, len)| {
            let end = start + len;
            !spans.iter().any(|&(a, l)| start <= a + l && a <= end)
        })
        .map(|(start, len)| Mark { start, len })
        .collect();
    marks.sort_by_key(|m| m.start);
    let mut apart: Vec<Mark> = Vec::with_capacity(marks.len());
    for m in marks {
        match apart.last_mut() {
            Some(last) if m.start <= last.start + last.len => {
                let end = (last.start + last.len).max(m.start + m.len);
                last.len = end - last.start;
            }
            _ => apart.push(m),
        }
    }

    let mut ranges: Vec<TextRange> = spans
        .iter()
        .map(|&(start, len)| TextRange {
            start,
            len,
            kind: RangeKind::Spoiler,
        })
        .collect();
    if let Some(formatted) = piece.formatted {
        ranges.extend(formatted.shown_mentions().into_iter().filter_map(|r| {
            let (start, end) = (r.start as usize, r.end()? as usize);
            if start < from || end > to {
                return None;
            }
            let (start, len) = span(start, end)?;
            Some(TextRange {
                start,
                len,
                kind: r.kind.clone(),
            })
        }));
    }
    ranges.sort_by_key(|r| r.start);

    if s[to..].chars().any(|c| !is_blank(c)) {
        text.push(ELLIPSIS);
    }
    Snippet {
        text,
        marks: apart,
        ranges,
        file: piece.file,
    }
}

#[cfg(test)]
mod tests;
