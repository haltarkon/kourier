//! Text with formatting ranges (architecture §3.6) and the UTF-8 ↔ UTF-16 offset map.
//!
//! Ranges are UTF-8 byte offsets in the core. [`Utf16Map`] converts at the Signal edge
//! (protobuf body ranges are UTF-16) and at the Qt edge (`QString` positions are UTF-16).
//! Ranges are validated on ingest with [`Text::sanitize`]: invalid ones are dropped, not fatal.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::features::FormatKind;
use crate::ids::UserId;

/// The object replacement character: one per mention in [`Text::body`].
pub const MENTION_PLACEHOLDER: char = '\u{FFFC}';

/// A message text: plain UTF-8 body plus formatting and mention ranges.
/// `Debug` hides the body and range payloads.
#[derive(Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Text {
    /// The text; each mention is one [`MENTION_PLACEHOLDER`].
    #[serde(default)]
    pub body: String,
    /// Formatting and mention ranges, UTF-8 byte offsets into `body`. May overlap.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ranges: Vec<TextRange>,
}

impl Text {
    /// Text without ranges.
    pub fn plain(body: impl Into<String>) -> Self {
        Self {
            body: body.into(),
            ranges: Vec::new(),
        }
    }

    /// Whether the body is empty.
    pub fn is_empty(&self) -> bool {
        self.body.is_empty()
    }

    /// Drops every range that [`validate_range`] rejects; returns how many were dropped.
    pub fn sanitize(&mut self) -> usize {
        let before = self.ranges.len();
        let body = &self.body;
        self.ranges.retain(|r| validate_range(body, r).is_ok());
        before - self.ranges.len()
    }

    /// The UTF-16 offset map of the body.
    pub fn utf16_map(&self) -> Utf16Map {
        Utf16Map::new(&self.body)
    }

    /// The text in pieces of at most `max_bytes` bytes each, in order, for a network whose
    /// messages carry no more (`ConversationFeatures::max_text_bytes`): a text that is too
    /// long for one message is sent as several instead of being refused.
    ///
    /// A piece ends at a line break where there is one in its second half, else at white
    /// space there, else between two characters; never inside a character. The pieces put
    /// together are the text again, except that a piece of nothing but white space is left
    /// out (no network takes an empty message). Ranges are kept and cut where a piece ends
    /// (a mention is one character and stays whole). A text that fits comes back as its only
    /// piece, and so does a text of white space alone; with `max_bytes` smaller than one
    /// character a piece is that one character.
    pub fn split(&self, max_bytes: usize) -> Vec<Text> {
        let body = self.body.as_str();
        if body.len() <= max_bytes {
            return vec![self.clone()];
        }
        let mut pieces = Vec::new();
        let mut start = 0;
        while start < body.len() {
            let end = piece_end(body, start, max_bytes);
            let piece = &body[start..end];
            if !piece.trim().is_empty() {
                let ranges = self
                    .ranges
                    .iter()
                    .filter_map(|r| {
                        let from = (r.start as usize).max(start);
                        let to = (r.end()? as usize).min(end);
                        (from < to).then_some(())?;
                        Some(TextRange {
                            start: u32::try_from(from - start).ok()?,
                            len: u32::try_from(to - from).ok()?,
                            kind: r.kind.clone(),
                        })
                    })
                    .collect();
                pieces.push(Text {
                    body: piece.to_owned(),
                    ranges,
                });
            }
            start = end;
        }
        if pieces.is_empty() {
            return vec![self.clone()];
        }
        pieces
    }
}

/// Where the piece of `body` that starts at `start` ends ([`Text::split`]): at most
/// `max_bytes` further on, on a character boundary, after the last line break or (failing
/// that) white space of its second half when the text goes on behind it.
fn piece_end(body: &str, start: usize, max_bytes: usize) -> usize {
    let mut end = start.saturating_add(max_bytes).min(body.len());
    if end == body.len() {
        return end;
    }
    while end > start && !body.is_char_boundary(end) {
        end -= 1;
    }
    if end == start {
        // Not even one character fits: it is not cut all the same.
        return start + body[start..].chars().next().map_or(1, char::len_utf8);
    }
    let window = &body[start..end];
    let half = window.len() / 2;
    let after = |(i, c): (usize, char)| i + c.len_utf8();
    let cut = window
        .rfind('\n')
        .filter(|i| *i >= half)
        .map(|i| i + 1)
        .or_else(|| {
            window
                .char_indices()
                .rev()
                .take_while(|(i, _)| *i >= half)
                .find(|(_, c)| c.is_whitespace())
                .map(after)
        });
    cut.map_or(end, |c| start + c)
}

impl fmt::Debug for Text {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Text {{ body: [redacted; {} bytes], ranges: {} }}",
            self.body.len(),
            self.ranges.len()
        )
    }
}

/// One range of a [`Text`], in UTF-8 bytes.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TextRange {
    /// Start, a UTF-8 byte offset into the body.
    pub start: u32,
    /// Length in UTF-8 bytes.
    pub len: u32,
    /// What the range means.
    pub kind: RangeKind,
}

impl TextRange {
    /// End offset (exclusive), `None` on overflow.
    pub fn end(&self) -> Option<u32> {
        self.start.checked_add(self.len)
    }
}

/// Meaning of a [`TextRange`]. `Debug` of `Link` hides the URL.
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RangeKind {
    /// Bold.
    Bold,
    /// Italic.
    Italic,
    /// Strikethrough.
    Strike,
    /// Hidden until clicked.
    Spoiler,
    /// Monospace.
    Mono,
    /// A mention of this user; covers one [`MENTION_PLACEHOLDER`].
    Mention(UserId),
    /// A mention of everyone; covers one [`MENTION_PLACEHOLDER`].
    MentionAll,
    /// A link to this URL.
    Link(String),
}

impl RangeKind {
    /// The feature this range needs (for `ConversationFeatures.formatting`).
    pub fn format_kind(&self) -> FormatKind {
        match self {
            Self::Bold => FormatKind::Bold,
            Self::Italic => FormatKind::Italic,
            Self::Strike => FormatKind::Strike,
            Self::Spoiler => FormatKind::Spoiler,
            Self::Mono => FormatKind::Mono,
            Self::Mention(_) => FormatKind::Mention,
            Self::MentionAll => FormatKind::MentionAll,
            Self::Link(_) => FormatKind::Link,
        }
    }

    fn is_mention(&self) -> bool {
        matches!(self, Self::Mention(_) | Self::MentionAll)
    }
}

impl fmt::Debug for RangeKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Mention(u) => write!(f, "Mention({u:?})"),
            Self::Link(_) => f.write_str("Link([redacted])"),
            other => fmt::Debug::fmt(&other.format_kind(), f),
        }
    }
}

/// Why a range is invalid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RangeError {
    /// Zero length.
    #[error("empty range")]
    Empty,
    /// Ends past the body (or overflows).
    #[error("range outside the body")]
    OutOfBounds,
    /// Starts or ends inside a UTF-8 character.
    #[error("range splits a character")]
    NotCharBoundary,
    /// A mention range that does not cover exactly one placeholder character.
    #[error("mention range does not cover one placeholder")]
    BadMention,
}

/// Checks one range against `body`: non-empty, inside the body, on char boundaries, and a
/// mention covers exactly one [`MENTION_PLACEHOLDER`].
pub fn validate_range(body: &str, range: &TextRange) -> Result<(), RangeError> {
    if range.len == 0 {
        return Err(RangeError::Empty);
    }
    let end = range.end().ok_or(RangeError::OutOfBounds)? as usize;
    let start = range.start as usize;
    if end > body.len() {
        return Err(RangeError::OutOfBounds);
    }
    if !body.is_char_boundary(start) || !body.is_char_boundary(end) {
        return Err(RangeError::NotCharBoundary);
    }
    if range.kind.is_mention() {
        let mut chars = body[start..end].chars();
        if chars.next() != Some(MENTION_PLACEHOLDER) || chars.next().is_some() {
            return Err(RangeError::BadMention);
        }
    }
    Ok(())
}

/// A character that is not one byte / one code unit (everything between two of them is ASCII).
#[derive(Clone, Copy, Debug)]
struct Wide {
    utf8: usize,
    utf16: usize,
    len8: usize,
    len16: usize,
}

/// Maps offsets of one string between UTF-8 bytes and UTF-16 code units.
///
/// Valid offsets are character boundaries: a UTF-8 offset inside a multi-byte character or a
/// UTF-16 offset between the two halves of a surrogate pair maps to `None`. Positions inside a
/// grapheme cluster (ZWJ sequences, flags, skin tones) are valid code-point boundaries in both
/// encodings and map normally. Lookups are `O(log n)` in the number of non-ASCII characters.
#[derive(Clone, Debug)]
pub struct Utf16Map {
    utf8_len: usize,
    utf16_len: usize,
    wide: Vec<Wide>,
}

impl Utf16Map {
    /// Builds the map for `s`.
    pub fn new(s: &str) -> Self {
        let mut wide = Vec::new();
        let mut utf16 = 0usize;
        for (utf8, c) in s.char_indices() {
            let (len8, len16) = (c.len_utf8(), c.len_utf16());
            if len8 > 1 {
                wide.push(Wide {
                    utf8,
                    utf16,
                    len8,
                    len16,
                });
            }
            utf16 += len16;
        }
        Self {
            utf8_len: s.len(),
            utf16_len: utf16,
            wide,
        }
    }

    /// Length in UTF-8 bytes.
    pub fn utf8_len(&self) -> usize {
        self.utf8_len
    }

    /// Length in UTF-16 code units (what `QString::size()` returns).
    pub fn utf16_len(&self) -> usize {
        self.utf16_len
    }

    /// UTF-8 byte offset → UTF-16 offset; `None` inside a character or past the end.
    pub fn to_utf16(&self, utf8: u32) -> Option<u32> {
        let o = utf8 as usize;
        if o > self.utf8_len {
            return None;
        }
        let i = self.wide.partition_point(|w| w.utf8 <= o);
        let r = match i.checked_sub(1).map(|i| self.wide[i]) {
            None => o,
            Some(w) if o == w.utf8 => w.utf16,
            Some(w) if o < w.utf8 + w.len8 => return None,
            Some(w) => w.utf16 + w.len16 + (o - (w.utf8 + w.len8)),
        };
        u32::try_from(r).ok()
    }

    /// UTF-16 offset → UTF-8 byte offset; `None` inside a surrogate pair or past the end.
    pub fn to_utf8(&self, utf16: u32) -> Option<u32> {
        let o = utf16 as usize;
        if o > self.utf16_len {
            return None;
        }
        let i = self.wide.partition_point(|w| w.utf16 <= o);
        let r = match i.checked_sub(1).map(|i| self.wide[i]) {
            None => o,
            Some(w) if o == w.utf16 => w.utf8,
            Some(w) if o < w.utf16 + w.len16 => return None,
            Some(w) => w.utf8 + w.len8 + (o - (w.utf16 + w.len16)),
        };
        u32::try_from(r).ok()
    }

    /// `(start, len)` in UTF-8 bytes → `(start, len)` in UTF-16 units.
    pub fn range_to_utf16(&self, start: u32, len: u32) -> Option<(u32, u32)> {
        let s = self.to_utf16(start)?;
        let e = self.to_utf16(start.checked_add(len)?)?;
        Some((s, e - s))
    }

    /// `(start, len)` in UTF-16 units → `(start, len)` in UTF-8 bytes.
    pub fn range_to_utf8(&self, start: u32, len: u32) -> Option<(u32, u32)> {
        let s = self.to_utf8(start)?;
        let e = self.to_utf8(start.checked_add(len)?)?;
        Some((s, e - s))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn range(start: u32, len: u32, kind: RangeKind) -> TextRange {
        TextRange { start, len, kind }
    }

    #[test]
    fn a_long_text_is_split_between_lines_words_and_characters() {
        // It fits: one piece, the text itself.
        let short = Text::plain("fits");
        assert_eq!(short.split(4), std::slice::from_ref(&short));
        assert_eq!(Text::default().split(0), [Text::default()]);

        // A line break in the second half of a piece is where it ends.
        let lines = Text::plain("first line\nsecond line\nthird");
        let got: Vec<String> = lines.split(16).into_iter().map(|t| t.body).collect();
        assert_eq!(got, ["first line\n", "second line\n", "third"]);
        assert_eq!(got.concat(), lines.body);

        // Without one, white space; the pieces together are the text again.
        let words = Text::plain("alpha beta gamma delta epsilon");
        let got: Vec<String> = words.split(12).into_iter().map(|t| t.body).collect();
        assert_eq!(got, ["alpha beta ", "gamma delta ", "epsilon"]);
        assert!(got.iter().all(|p| p.len() <= 12));

        // Without white space, between characters, never inside one.
        let emoji = Text::plain("🎉🎉🎉🎉🎉");
        let got: Vec<String> = emoji.split(10).into_iter().map(|t| t.body).collect();
        assert_eq!(got, ["🎉🎉", "🎉🎉", "🎉"]);
        let got: Vec<String> = emoji.split(3).into_iter().map(|t| t.body).collect();
        assert_eq!(
            got.len(),
            5,
            "a limit below one character still makes progress"
        );

        // A piece of white space alone is left out; a text of nothing else stays one piece.
        let gap = Text::plain(format!("word{}word", " ".repeat(40)));
        let got: Vec<String> = gap.split(10).into_iter().map(|t| t.body).collect();
        assert_eq!(got.first().map(String::as_str), Some("word      "));
        assert_eq!(got.last().map(String::as_str), Some("    word"));
        assert_eq!(
            got.len(),
            2,
            "the three pieces of spaces between them are left out"
        );
        let blank = Text::plain(" ".repeat(30));
        assert_eq!(blank.split(10), std::slice::from_ref(&blank));
    }

    #[test]
    fn ranges_follow_their_pieces() {
        // "aaaa bbbb \u{FFFC} cccc dddd": bold over "bbbb @ cccc", a mention in the middle.
        let body = format!("aaaa bbbb {MENTION_PLACEHOLDER} cccc dddd");
        let user = UserId::parse("aci:00000000-0000-4000-8000-0000000a11ce").expect("user");
        let text = Text {
            ranges: vec![
                range(5, 13, RangeKind::Bold),
                range(10, 3, RangeKind::Mention(user.clone())),
                range(19, 4, RangeKind::Italic),
            ],
            body,
        };
        assert!(
            text.ranges
                .iter()
                .all(|r| validate_range(&text.body, r).is_ok())
        );
        let pieces = text.split(12);
        let bodies: Vec<&str> = pieces.iter().map(|p| p.body.as_str()).collect();
        assert_eq!(bodies, ["aaaa bbbb ", "\u{FFFC} cccc ", "dddd"]);
        assert_eq!(pieces[0].ranges, [range(5, 5, RangeKind::Bold)]);
        assert_eq!(
            pieces[1].ranges,
            [
                range(0, 8, RangeKind::Bold),
                range(0, 3, RangeKind::Mention(user))
            ]
        );
        assert_eq!(pieces[2].ranges, [range(0, 4, RangeKind::Italic)]);
        for p in &pieces {
            assert!(p.body.len() <= 12);
            assert!(
                p.ranges.iter().all(|r| validate_range(&p.body, r).is_ok()),
                "every range of a piece is valid in it"
            );
        }
    }

    #[test]
    fn ascii_is_identity() {
        let m = Utf16Map::new("hello");
        for i in 0..=5 {
            assert_eq!(m.to_utf16(i), Some(i));
            assert_eq!(m.to_utf8(i), Some(i));
        }
        assert_eq!(m.to_utf16(6), None);
        assert_eq!(m.to_utf8(6), None);
    }

    #[test]
    fn astral_emoji_and_zwj() {
        // "a😀b": 😀 is 4 bytes / 2 units.
        let m = Utf16Map::new("a😀b");
        assert_eq!((m.utf8_len(), m.utf16_len()), (6, 4));
        assert_eq!(m.to_utf16(1), Some(1));
        assert_eq!(m.to_utf16(2), None);
        assert_eq!(m.to_utf16(5), Some(3));
        assert_eq!(m.to_utf8(2), None, "inside the surrogate pair");
        assert_eq!(m.to_utf8(3), Some(5));
        // Family ZWJ sequence: 4 astral people + 3 ZWJ (3 bytes / 1 unit each).
        let fam = "👨\u{200D}👩\u{200D}👧\u{200D}👦";
        let m = Utf16Map::new(fam);
        assert_eq!(m.utf8_len(), fam.len());
        assert_eq!(m.utf16_len(), fam.encode_utf16().count());
        assert_eq!(m.range_to_utf16(0, fam.len() as u32), Some((0, 11)));
        assert_eq!(m.range_to_utf8(0, 11), Some((0, fam.len() as u32)));
    }

    #[test]
    fn validation_rules() {
        let body = format!("hi {MENTION_PLACEHOLDER} é!");
        let p = 3u32;
        let ok = [
            range(0, 2, RangeKind::Bold),
            range(p, 3, RangeKind::MentionAll),
            range(p + 4, 2, RangeKind::Italic),
        ];
        for r in &ok {
            assert_eq!(validate_range(&body, r), Ok(()), "{r:?}");
        }
        let bad = [
            (range(0, 0, RangeKind::Bold), RangeError::Empty),
            (range(0, 99, RangeKind::Bold), RangeError::OutOfBounds),
            (range(u32::MAX, 2, RangeKind::Bold), RangeError::OutOfBounds),
            (
                range(p + 1, 1, RangeKind::Bold),
                RangeError::NotCharBoundary,
            ),
            (range(0, 2, RangeKind::MentionAll), RangeError::BadMention),
            (
                range(p - 1, 4, RangeKind::MentionAll),
                RangeError::BadMention,
            ),
        ];
        for (r, e) in &bad {
            assert_eq!(validate_range(&body, r), Err(*e), "{r:?}");
        }
        let mut t = Text {
            body,
            ranges: ok
                .iter()
                .chain(bad.iter().map(|(r, _)| r))
                .cloned()
                .collect(),
        };
        assert_eq!(t.sanitize(), bad.len());
        assert_eq!(t.ranges, ok);
    }

    #[test]
    fn debug_hides_body_and_links() {
        let t = Text {
            body: "meet at the secret place".into(),
            ranges: vec![range(
                0,
                4,
                RangeKind::Link("https://secret.example".into()),
            )],
        };
        let s = format!("{t:?} {:?}", t.ranges);
        assert!(!s.contains("secret"), "{s}");
    }
}
