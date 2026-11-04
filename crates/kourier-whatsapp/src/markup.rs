//! WhatsApp's text formatting and mentions -> Kourier's `Text` (W3b, chunk C3; architecture §3.6).
//!
//! PURE: no I/O, no clock, no logging, no panic on any input. The input is the text of a message (or a
//! media caption) and the people its context info mentions; the output is a [`Text`] whose body has the
//! formatting markers REMOVED (WhatsApp shows the text without them) and whose ranges are UTF-8 byte
//! offsets, plus whether we are among the mentioned.
//!
//! The rules are WhatsApp's own as its help page ("How to format your messages") states them; no
//! reference parser is on disk, so they are the lead's reading of that page and bounded by tests:
//! - `*bold*`, `_italic_`, `~strike~`, `` `mono` `` and ```` ```block``` ````.
//! - A pair counts only when the opening marker is at the start or after a character that is not a letter
//!   or digit (white space, punctuation, another marker) and is followed by a character that is not white
//!   space; the closing marker follows a character that is not white space and is followed by the end or a
//!   character that is not a letter or digit. So `a*b*c` stays text, `(*bold*)` is bold.
//! - A single-character marker never spans a line break; a block of three backticks may.
//! - Inside mono nothing else is parsed; other markers nest (`*_both_*`), to a depth of [`MAX_DEPTH`].
//! - A marker that finds no partner stays text. List lines (`- `, `* `, `1. `) and quote lines (`> `) stay as
//!   typed: the contract has no list or quote range.
//!
//! Mentions come first: for each mentioned person, in order, the first not yet used `@<handle>` in the text
//! (the handle being the user part of the person's id, followed by something that is not a letter or digit)
//! becomes ONE U+FFFC covered by a `Mention` range; a person whose `@handle` is not in the text is dropped
//! (WhatsApp's clients do the same). `mentions_me` is true when a mentioned person is one of our own ids, in
//! the text or not. A community's group mention and a `nonJidMentions` entry stay plain text (the contract has
//! no range for them).
//!
//! A wrong rule shows a stray marker or misses one; it never loses a character that is not a marker.

use kourier_model::text::MENTION_PLACEHOLDER;
use kourier_model::{RangeKind, Text, TextRange, UserId};

/// Most mentions read from one message.
const MAX_MENTIONS: usize = 1_000;
/// Deepest nesting of different markers (a deeper one stays text): bounds the recursion.
const MAX_DEPTH: usize = 6;

/// One person a message mentions.
#[derive(Clone)]
pub(crate) struct Mentioned {
    /// The user part of the person's id as the text spells it after `@`.
    pub(crate) handle: String,
    /// The person.
    pub(crate) user: UserId,
    /// The person is one of our own ids.
    pub(crate) me: bool,
}

/// The text of a message with its mentions and formatting read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Marked {
    pub(crate) text: Text,
    /// A mentioned person is one of our own ids.
    pub(crate) mentions_me: bool,
}

/// One position of the text before formatting: a character or a mention that took its place.
#[derive(Clone, Copy)]
enum Atom {
    Ch(char),
    /// Index into the accepted mentions.
    Mention(usize),
}

impl Atom {
    fn is_space(self) -> bool {
        matches!(self, Self::Ch(c) if c.is_whitespace())
    }

    /// A letter or a digit (a mention counts as one: it is a name).
    fn is_word(self) -> bool {
        match self {
            Self::Ch(c) => c.is_alphanumeric(),
            Self::Mention(_) => true,
        }
    }

    fn is_newline(self) -> bool {
        matches!(self, Self::Ch('\n' | '\r'))
    }
}

/// The four single-character markers.
const MARKERS: [char; 4] = ['*', '_', '~', '`'];

fn marker_index(atom: Atom) -> Option<usize> {
    match atom {
        Atom::Ch(c) => MARKERS.iter().position(|m| *m == c),
        Atom::Mention(_) => None,
    }
}

fn kind_of(marker: usize) -> RangeKind {
    match marker {
        0 => RangeKind::Bold,
        1 => RangeKind::Italic,
        2 => RangeKind::Strike,
        _ => RangeKind::Mono,
    }
}

/// Reads `text` and `mentioned`.
pub(crate) fn mark_up(text: &str, mentioned: &[Mentioned]) -> Marked {
    let mentions_me = mentioned.iter().any(|m| m.me);
    let (atoms, accepted) = place_mentions(text, mentioned);
    let parser = Parser::new(&atoms, &accepted);
    let (body, ranges) = parser.run();
    Marked {
        text: Text { body, ranges },
        mentions_me,
    }
}

/// The text as atoms, with each mentioned person's first free `@handle` replaced by a mention atom.
/// Returns the atoms and the people who found a place, in the order of the atoms' indexes.
fn place_mentions(text: &str, mentioned: &[Mentioned]) -> (Vec<Atom>, Vec<UserId>) {
    let mut atoms: Vec<Atom> = text.chars().map(Atom::Ch).collect();
    let mut accepted: Vec<UserId> = Vec::new();
    for person in mentioned.iter().take(MAX_MENTIONS) {
        let handle: Vec<char> = person.handle.chars().collect();
        if handle.is_empty() {
            continue;
        }
        let Some(at) = find_handle(&atoms, &handle) else {
            continue;
        };
        let index = accepted.len();
        accepted.push(person.user.clone());
        // `@` and the handle become one atom.
        atoms.splice(
            at..at + 1 + handle.len(),
            std::iter::once(Atom::Mention(index)),
        );
    }
    (atoms, accepted)
}

/// The first `@handle` in `atoms` that is a whole word.
fn find_handle(atoms: &[Atom], handle: &[char]) -> Option<usize> {
    let span = 1 + handle.len();
    if atoms.len() < span {
        return None;
    }
    (0..=atoms.len() - span).find(|&i| {
        matches!(atoms[i], Atom::Ch('@'))
            && handle
                .iter()
                .enumerate()
                .all(|(k, c)| matches!(atoms[i + 1 + k], Atom::Ch(x) if x == *c))
            && atoms.get(i + span).is_none_or(|next| !next.is_word())
    })
}

struct Parser<'a> {
    atoms: &'a [Atom],
    users: &'a [UserId],
    /// For each index `i` (and `n`): the index of the first line break at or after `i`, `n` if none.
    next_newline: Vec<usize>,
    /// For each index `i` (and `n`): the index of the first character that is not white space at or
    /// after `i`, `n` if none.
    next_ink: Vec<usize>,
    /// Per marker: the places where a pair may CLOSE (it follows a non-space and is followed by the end
    /// or a non-word), sorted.
    closers: [Vec<usize>; 4],
    /// The places where three backticks start, sorted.
    fences: Vec<usize>,
    body: String,
    ranges: Vec<TextRange>,
}

impl<'a> Parser<'a> {
    fn new(atoms: &'a [Atom], users: &'a [UserId]) -> Self {
        let n = atoms.len();
        let mut next_newline = vec![n; n + 1];
        let mut next_ink = vec![n; n + 1];
        for i in (0..n).rev() {
            next_newline[i] = if atoms[i].is_newline() {
                i
            } else {
                next_newline[i + 1]
            };
            next_ink[i] = if atoms[i].is_space() {
                next_ink[i + 1]
            } else {
                i
            };
        }
        let mut closers: [Vec<usize>; 4] = Default::default();
        let mut fences = Vec::new();
        for (j, atom) in atoms.iter().enumerate() {
            let Some(marker) = marker_index(*atom) else {
                continue;
            };
            let after_ink = j > 0 && !atoms[j - 1].is_space();
            let before_boundary = atoms.get(j + 1).is_none_or(|next| !next.is_word());
            if after_ink && before_boundary {
                closers[marker].push(j);
            }
            if marker == 3
                && matches!(atoms.get(j + 1), Some(Atom::Ch('`')))
                && matches!(atoms.get(j + 2), Some(Atom::Ch('`')))
            {
                fences.push(j);
            }
        }
        Self {
            atoms,
            users,
            next_newline,
            next_ink,
            closers,
            fences,
            body: String::with_capacity(n),
            ranges: Vec::new(),
        }
    }

    fn run(mut self) -> (String, Vec<TextRange>) {
        let n = self.atoms.len();
        self.parse(0, n, 0);
        (self.body, self.ranges)
    }

    /// Emits `atoms[from..to]` with its pairs read.
    fn parse(&mut self, from: usize, to: usize, depth: usize) {
        let mut i = from;
        while i < to {
            let atom = self.atoms[i];
            if let Some(end) = self.block_at(i, to) {
                // ```block```: the content as typed, nothing parsed inside.
                self.emit_range(i + 3, end, RangeKind::Mono, None);
                i = end + 3;
            } else if let Some((marker, close)) = self.pair_at(i, to, depth) {
                let kind = kind_of(marker);
                let nested = (marker != 3).then_some(depth + 1);
                self.emit_range(i + 1, close, kind, nested);
                i = close + 1;
            } else {
                self.emit(atom);
                i += 1;
            }
        }
    }

    /// The closing fence of a block that opens at `i`, when there is one in `..to` and something but
    /// white space lies between.
    fn block_at(&self, i: usize, to: usize) -> Option<usize> {
        if self.fences.binary_search(&i).is_err() {
            return None;
        }
        let from = i + 3;
        let k = self.fences.partition_point(|f| *f < from);
        let end = *self.fences.get(k)?;
        (end + 3 <= to && self.next_ink[from] < end).then_some(end)
    }

    /// The marker and the close of a pair that opens at `i`.
    fn pair_at(&self, i: usize, to: usize, depth: usize) -> Option<(usize, usize)> {
        if depth >= MAX_DEPTH {
            return None;
        }
        let marker = marker_index(self.atoms[i])?;
        // A backtick followed by another is a fence that found no partner, not an inline pair.
        let opens = (i == 0 || !self.atoms[i - 1].is_word())
            && i + 1 < to
            && !self.atoms[i + 1].is_space()
            && !(marker == 3 && matches!(self.atoms[i + 1], Atom::Ch('`')));
        if !opens {
            return None;
        }
        // The first valid closer after one character of content, on the same line.
        let closers = &self.closers[marker];
        let k = closers.partition_point(|j| *j < i + 2);
        let close = *closers.get(k)?;
        (close < to && self.next_newline[i + 1] >= close).then_some((marker, close))
    }

    /// Emits the content `from..to` as one range of `kind`; `nested` parses it at that depth, `None`
    /// copies it.
    fn emit_range(&mut self, from: usize, to: usize, kind: RangeKind, nested: Option<usize>) {
        let start = self.body.len();
        match nested {
            Some(depth) => self.parse(from, to, depth),
            None => {
                for k in from..to {
                    self.emit(self.atoms[k]);
                }
            }
        }
        let len = self.body.len() - start;
        if let (Ok(start), Ok(len)) = (u32::try_from(start), u32::try_from(len))
            && len > 0
        {
            self.ranges.push(TextRange { start, len, kind });
        }
    }

    fn emit(&mut self, atom: Atom) {
        match atom {
            Atom::Ch(c) => self.body.push(c),
            Atom::Mention(index) => {
                let start = self.body.len();
                self.body.push(MENTION_PLACEHOLDER);
                if let (Ok(start), Some(user)) = (u32::try_from(start), self.users.get(index)) {
                    self.ranges.push(TextRange {
                        start,
                        len: u32::try_from(MENTION_PLACEHOLDER.len_utf8()).unwrap_or(3),
                        kind: RangeKind::Mention(user.clone()),
                    });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
