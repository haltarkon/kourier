//! The fold of the emoji search: what a typed query and the generated table's search words both go
//! through, so that the two meet. It is also compiled into `cargo xtask emoji` (`#[path]`), so it
//! uses `std` only.
//!
//! A text is lower-cased, its Latin accents are taken off (`é` is `e`, `ł` is `l`, `ß` is `ss`),
//! `ё` is `е` and `ґ` is `г`, and apostrophes and combining marks after a Latin letter go. Other
//! letters (Cyrillic `й`, `ї`, Hebrew, Arabic, ...) stay as they are. A word is a run of
//! letters and digits.

/// Latin letters with accents, and the plain letter each one stands for.
const LATIN: [(&str, &str); 19] = [
    ("àáâãäåāăą", "a"),
    ("çćčĉċ", "c"),
    ("ďđ", "d"),
    ("èéêëēĕėęě", "e"),
    ("ğĝġģ", "g"),
    ("ĥħ", "h"),
    ("ìíîïĩīĭįı", "i"),
    ("ĵ", "j"),
    ("ķ", "k"),
    ("ĺļľŀł", "l"),
    ("ñńņň", "n"),
    ("òóôõöøōŏő", "o"),
    ("ŕŗř", "r"),
    ("śŝşš", "s"),
    ("ţťŧ", "t"),
    ("ùúûüũūŭůűų", "u"),
    ("ŵ", "w"),
    ("ýÿŷ", "y"),
    ("źżž", "z"),
];

/// The fold of `text`: see the module's words.
pub fn fold(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars().flat_map(char::to_lowercase) {
        match c {
            // Apostrophes (the Ukrainian one is written four ways) are no part of a word.
            '\'' | '’' | 'ʼ' | '`' | '′' => {}
            // A combining mark after a Latin letter is an accent in the decomposed form.
            '\u{0300}'..='\u{036f}'
                if out.chars().last().is_some_and(|p| p.is_ascii_alphabetic()) => {}
            'ß' => out.push_str("ss"),
            'æ' => out.push_str("ae"),
            'œ' => out.push_str("oe"),
            'ё' => out.push('е'),
            'ґ' => out.push('г'),
            c if c.is_ascii() => out.push(c),
            c => match LATIN.iter().find(|(from, _)| from.contains(c)) {
                Some((_, to)) => out.push_str(to),
                None => out.push(c),
            },
        }
    }
    out
}

/// The words of `text` after the fold: runs of letters and digits.
pub fn words(text: &str) -> Vec<String> {
    fold(text)
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_owned)
        .collect()
}
