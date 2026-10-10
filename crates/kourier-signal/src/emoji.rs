//! What a reaction may carry: exactly one emoji.
//!
//! Desktop drops a received reaction whose `emoji` is not one emoji of its table
//! (`SD/ts/background.preload.ts`, "Received an invalid reaction emoji"; `Emoji.isEmoji` in
//! `SD/ts/axo/emoji.std.ts`), so a contact cannot pin a word, a sentence or a link under
//! somebody's message as a "reaction". Kourier keeps that rule on both ways: a reaction that
//! is not one emoji is dropped on receive and refused on send (the other clients would drop it,
//! and it would show here alone).
//!
//! Kourier has no emoji table. [`is_one`] checks the shape of the Unicode emoji sequences
//! instead (UTS #51): one emoji character with its variation selector, skin tone or tag
//! sequence; several of them joined by zero-width joiners (families, professions, the pirate
//! and rainbow flags); a keycap; or a flag of two regional indicators. An emoji newer than this
//! build still passes, which a table would refuse; a text never does.

/// The longest reaction taken, in bytes. The longest emoji sequences Unicode lists (a kiss or a
/// family of four with skin tones, a subdivision flag) stay under 50 bytes.
const MAX_BYTES: usize = 64;

/// Zero-width joiner: glues emoji into one (`👩‍🚀`).
const ZWJ: char = '\u{200D}';
/// Combining enclosing keycap (`1️⃣`).
const KEYCAP: char = '\u{20E3}';

/// A character that is an emoji by itself (the blocks and single characters Unicode's
/// `emoji-data.txt` marks `Emoji`, without the ASCII digits, `#` and `*` of keycaps).
fn is_base(c: char) -> bool {
    matches!(
        u32::from(c),
        0x1F000..=0x1FAFF
            | 0x2600..=0x27BF
            | 0x2300..=0x23FF
            | 0x2B00..=0x2BFF
            | 0x2190..=0x21FF
            | 0x25A0..=0x25FF
            | 0x2934
            | 0x2935
            | 0x24C2
            | 0x3030
            | 0x303D
            | 0x3297
            | 0x3299
            | 0x00A9
            | 0x00AE
            | 0x203C
            | 0x2049
            | 0x2122
            | 0x2139
    )
}

/// A character that changes the emoji before it: a variation selector, a skin tone, or a tag
/// of a subdivision flag (`🏴󠁧󠁢󠁥󠁮󠁧󠁿`).
fn is_modifier(c: char) -> bool {
    matches!(
        u32::from(c),
        0xFE0E | 0xFE0F | 0x1F3FB..=0x1F3FF | 0xE0020..=0xE007F
    )
}

/// A regional indicator; two of them are a flag.
fn is_regional(c: char) -> bool {
    matches!(u32::from(c), 0x1F1E6..=0x1F1FF)
}

/// Whether `text` is exactly one emoji (module docs).
pub(crate) fn is_one(text: &str) -> bool {
    if text.is_empty() || text.len() > MAX_BYTES {
        return false;
    }
    let mut chars = text.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if matches!(first, '0'..='9' | '#' | '*') {
        // A keycap: the key, an optional emoji presentation selector, the keycap.
        let rest: Vec<char> = chars.collect();
        return matches!(rest.as_slice(), [KEYCAP] | ['\u{FE0F}', KEYCAP]);
    }
    if is_regional(first) {
        return chars.next().is_some_and(is_regional) && chars.next().is_none();
    }
    if !is_base(first) {
        return false;
    }
    // One emoji, its modifiers, and further emoji only behind a joiner.
    let mut after_joiner = false;
    for c in chars {
        if after_joiner {
            if !is_base(c) || is_regional(c) {
                return false;
            }
            after_joiner = false;
        } else if c == ZWJ {
            after_joiner = true;
        } else if !is_modifier(c) {
            return false;
        }
    }
    !after_joiner
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_emoji_of_every_shape_passes() {
        for emoji in [
            "👍",
            "❤️",
            "❤",
            "😀",
            "🫠",
            "👍🏽",
            "👨‍👩‍👧‍👦",
            "🧑🏻‍❤️‍💋‍🧑🏿",
            "👩‍🚀",
            "⛹🏽‍♂️",
            "🏳️‍🌈",
            "🏴‍☠️",
            "🏴󠁧󠁢󠁥󠁮󠁧󠁿",
            "🇺🇦",
            "1️⃣",
            "#️⃣",
            "*\u{20E3}",
            "©️",
            "™️",
            "‼️",
            "ℹ️",
            "↔️",
            "⌚",
            "⏳",
            "Ⓜ️",
            "▶️",
            "◻️",
            "⤴️",
            "⬅️",
            "⭐",
            "〰️",
            "㊗️",
            "🀄",
            "🅰️",
            "🈚",
            "👁️‍🗨️",
            "☺\u{FE0E}",
        ] {
            assert!(is_one(emoji), "{emoji:?}");
        }
    }

    #[test]
    fn text_several_emoji_and_broken_sequences_do_not() {
        let long = "👍".repeat(40);
        for not in [
            "",
            " ",
            "a",
            "ok",
            "1",
            "#",
            "12\u{20E3}",
            "1\u{FE0F}",
            "👍👍",
            "👍 ",
            " 👍",
            "👍a",
            "a👍",
            "👍\n",
            "https://example.org",
            "<b>👍</b>",
            "\u{200D}",
            "\u{FE0F}",
            "👍\u{200D}",
            "\u{200D}👍",
            "👩\u{200D}\u{200D}🚀",
            "👩\u{200D}a",
            "🇺",
            "🇺🇦🇺",
            "🇺🇦🇺🇦",
            "🇺a",
            "👩\u{200D}🇺",
            "\u{202E}👍",
            "👍\u{0301}",
            long.as_str(),
        ] {
            assert!(!is_one(not), "{not:?}");
        }
    }
}
