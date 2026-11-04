//! A typed phone number made digits (TG-2 brief §3.4): the rules WhatsApp's login uses
//! (`kourier-whatsapp/src/link.rs` `normalize_number`), written again for this crate: a leading
//! `+` or `00` is the international prefix, separators (spaces, `-`, `.`, parentheses) are dropped,
//! 7..=15 digits (E.164), and no leading `0` once the prefix is gone (the country code is
//! missing). The digits are personal data: they live in a zeroizing buffer and are never printed.

use zeroize::Zeroizing;

/// The digits of a phone number with its country code: at least 7, at most 15 (E.164).
pub(crate) const NUMBER_DIGITS: std::ops::RangeInclusive<usize> = 7..=15;

/// Why a typed number is no number (fixed names only).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NumberProblem {
    /// A character that is neither a digit nor a separator.
    Characters,
    TooShort,
    TooLong,
    /// It starts with `0` after the international prefix is gone: the country code is missing.
    LeadingZero,
}

/// `typed` as digits only (no `+`), or why it is no number.
pub(crate) fn normalize(typed: &str) -> Result<Zeroizing<String>, NumberProblem> {
    let trimmed = typed.trim();
    let (plus, rest) = match trimmed.strip_prefix('+') {
        Some(rest) => (true, rest),
        None => (false, trimmed),
    };
    // Reserved once at the most the input can hold: no growth, so no copy is left behind.
    let mut digits: Zeroizing<String> = Zeroizing::new(String::with_capacity(rest.len()));
    for c in rest.chars() {
        match c {
            '0'..='9' => digits.push(c),
            '-' | '.' | '(' | ')' => {}
            c if c.is_whitespace() => {}
            _ => return Err(NumberProblem::Characters),
        }
    }
    let start = if !plus && digits.starts_with("00") {
        2
    } else {
        0
    };
    let body = &digits[start..];
    if body.starts_with('0') {
        return Err(NumberProblem::LeadingZero);
    }
    match body.len() {
        n if n < *NUMBER_DIGITS.start() => Err(NumberProblem::TooShort),
        n if n > *NUMBER_DIGITS.end() => Err(NumberProblem::TooLong),
        _ if start == 0 => Ok(digits),
        _ => {
            let mut out: Zeroizing<String> = Zeroizing::new(String::with_capacity(body.len()));
            out.push_str(body);
            Ok(out)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rules of WhatsApp's `normalize_number`, re-stated for Telegram, over synthetic numbers.
    #[test]
    fn normalize_takes_the_international_forms_and_refuses_the_rest() {
        let ok = |typed: &str| normalize(typed).map(|d| d.as_str().to_owned());
        for (typed, want) in [
            ("+999068204719", "999068204719"),
            ("999068204719", "999068204719"),
            ("00999068204719", "999068204719"),
            (" +999 06-82.04 (719) ", "999068204719"),
            ("+9990682", "9990682"),
            ("+999068204719123", "999068204719123"),
        ] {
            assert_eq!(
                ok(typed),
                Ok(want.to_owned()),
                "a case of {} chars",
                typed.len()
            );
        }
        for (typed, want) in [
            ("+999 06a8", NumberProblem::Characters),
            ("+999/068204719", NumberProblem::Characters),
            ("", NumberProblem::TooShort),
            ("+999068", NumberProblem::TooShort),
            ("+9990682047191234", NumberProblem::TooLong),
            ("0999068204719", NumberProblem::LeadingZero),
            ("000999068204719", NumberProblem::LeadingZero),
            // A `+` keeps the `00`: it is the start of the number, which then starts with 0.
            ("+00999068204719", NumberProblem::LeadingZero),
        ] {
            assert_eq!(ok(typed), Err(want), "a case of {} chars", typed.len());
        }
    }
}
