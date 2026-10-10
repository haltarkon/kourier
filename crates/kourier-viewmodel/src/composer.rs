//! Composer helpers: outgoing text normalisation and UTF-16 ↔ UTF-8 positions between
//! `QString` (UTF-16) and the model's UTF-8 ranges, through [`kourier_model::text::Utf16Map`].

use kourier_model::text::{MENTION_PLACEHOLDER, Utf16Map};
use kourier_model::{Text, TextRange};

/// Turns composer text into an outgoing [`Text`]: `\r\n` → `\n`, stray mention placeholders
/// and control characters (except newline and tab) removed, trailing whitespace trimmed.
/// Returns `None` when nothing is left to send.
pub fn outgoing_text(raw: &str) -> Option<Text> {
    let body: String = raw
        .replace("\r\n", "\n")
        .chars()
        .filter(|&c| c == '\n' || c == '\t' || !(c.is_control() || c == MENTION_PLACEHOLDER))
        .collect();
    let body = body.trim_end();
    if body.trim_start().is_empty() {
        return None;
    }
    Some(Text::plain(body))
}

/// A `QString` position (UTF-16) in `s` → UTF-8 byte offset; a position inside a surrogate
/// pair snaps back to the start of the character.
pub fn utf16_to_utf8(s: &str, pos16: u32) -> usize {
    let map = Utf16Map::new(s);
    let pos16 = pos16.min(map.utf16_len() as u32);
    let mut p = pos16;
    loop {
        if let Some(o) = map.to_utf8(p) {
            return o as usize;
        }
        // Only the second half of a surrogate pair fails; p > 0 there.
        p -= 1;
    }
}

/// A UTF-8 byte offset in `s` → `QString` position; an offset inside a character snaps back.
pub fn utf8_to_utf16(s: &str, mut pos8: usize) -> u32 {
    pos8 = pos8.min(s.len());
    while !s.is_char_boundary(pos8) {
        pos8 -= 1;
    }
    Utf16Map::new(s).to_utf16(pos8 as u32).unwrap_or_default()
}

/// Converts a range given in UTF-16 units (a `TextEdit` selection) into a model range on `text`;
/// `None` when it splits a surrogate pair or leaves the body.
pub fn range_from_utf16(
    text: &Text,
    start16: u32,
    len16: u32,
    kind: kourier_model::RangeKind,
) -> Option<TextRange> {
    let (start, len) = text.utf16_map().range_to_utf8(start16, len16)?;
    Some(TextRange { start, len, kind })
}

#[cfg(test)]
mod tests {
    use super::*;
    use kourier_model::RangeKind;

    #[test]
    fn outgoing_normalisation() {
        assert_eq!(outgoing_text("  \n\t "), None);
        assert_eq!(outgoing_text(""), None);
        assert_eq!(
            outgoing_text("hi\r\nthere \u{7}\u{FFFC}\n\n").map(|t| t.body),
            Some("hi\nthere".to_owned())
        );
        assert_eq!(
            outgoing_text("  indented").map(|t| t.body),
            Some("  indented".to_owned())
        );
    }

    #[test]
    fn positions_with_emoji() {
        // "a😀b": 😀 = 4 bytes / 2 units.
        let s = "a😀b";
        assert_eq!(utf16_to_utf8(s, 0), 0);
        assert_eq!(utf16_to_utf8(s, 1), 1);
        assert_eq!(utf16_to_utf8(s, 2), 1, "inside the pair snaps back");
        assert_eq!(utf16_to_utf8(s, 3), 5);
        assert_eq!(utf16_to_utf8(s, 99), 6);
        assert_eq!(utf8_to_utf16(s, 3), 1);
        assert_eq!(utf8_to_utf16(s, 5), 3);
        assert_eq!(utf8_to_utf16(s, 99), 4);
        let fam = "x👨\u{200D}👩\u{200D}👧y";
        let t = Text::plain(fam);
        let r = range_from_utf16(&t, 1, 8, RangeKind::Bold).expect("range");
        assert_eq!(
            &fam[r.start as usize..(r.start + r.len) as usize],
            "👨\u{200D}👩\u{200D}👧"
        );
        assert!(range_from_utf16(&t, 2, 1, RangeKind::Bold).is_none());
    }
}
