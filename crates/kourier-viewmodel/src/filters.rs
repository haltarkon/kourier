//! The RegExp filters' side of Settings and the filter dialog (RegExp filters R2). Qt-free: the
//! rows of the list, the live check of a pattern worded in Kourier's language, the wording of
//! the core's refusals, the eight swatches and the defaults of a new filter. The front end only
//! draws them (`App.filters`). The patterns and names are the user's: nothing here is logged
//! ([`FilterRow`]'s `Debug` leaves them out) and a pattern never goes into an error text.

use kourier_core::facade::filters::{Field, PatternError, PatternErrorKind};
use kourier_core::facade::{Filter, FilterRefusal, FilterSpec, filters};
use kourier_model::Redacted;

use crate::diff::RoleSet;
use crate::i18n::{trc, trfc};

/// The eight swatches of a filter's colour (`0xRRGGBB`): a marker yellow first, then orange,
/// red, pink, violet, blue, teal and green. R3 picks the alpha per background so the text
/// stays readable in light, dark and high contrast.
pub const SWATCHES: [u32; 8] = [
    0xFFD60A, 0xFF9F0A, 0xFF453A, 0xFF6EB4, 0xBF5AF2, 0x0A84FF, 0x1FC8B4, 0x30D158,
];

/// One saved filter as Settings lists it.
#[derive(Clone, PartialEq, Eq)]
pub struct FilterRow {
    /// The filter's id (`FilterId.0`): the row's key.
    pub id: i64,
    /// The name.
    pub name: String,
    /// The fields it looks at, worded ("Message text, Chat's name").
    pub fields: String,
    /// The colour, `0xRRGGBB`.
    pub colour: u32,
    /// Highlight is on (the row shows its colour dot).
    pub highlight: bool,
    /// Notify is on.
    pub notify: bool,
    /// "Also in muted chats".
    pub notify_muted: bool,
    /// Match Case.
    pub match_case: bool,
    /// The patterns, for the dialog's prefill (empty: the field is not used).
    pub text: String,
    /// The sender's pattern.
    pub sender: String,
    /// The chat's pattern.
    pub chat: String,
    /// The filter is the first of the list (Move Up is not offered).
    pub first: bool,
    /// The filter is the last of the list (Move Down is not offered).
    pub last: bool,
}

crate::safe_debug!(FilterRow {
    id,
    colour,
    highlight,
    notify,
    notify_muted,
    match_case,
    first,
    last
});

/// Role indices of the list model (`App.filters`).
pub mod filter_role {
    /// The id (the row's key).
    pub const ID: u32 = 0;
    /// The name.
    pub const NAME: u32 = 1;
    /// The fields used, worded.
    pub const FIELDS: u32 = 2;
    /// The colour.
    pub const COLOUR: u32 = 3;
    /// Highlight.
    pub const HIGHLIGHT: u32 = 4;
    /// Notify.
    pub const NOTIFY: u32 = 5;
    /// Also in muted chats.
    pub const NOTIFY_MUTED: u32 = 6;
    /// First of the list.
    pub const FIRST: u32 = 7;
    /// Last of the list.
    pub const LAST: u32 = 8;
}

impl FilterRow {
    /// The row's identity in a list model.
    pub fn key(&self) -> i64 {
        self.id
    }

    /// Which roles differ between two versions of the same filter's row.
    pub fn changed_roles(a: &Self, b: &Self) -> RoleSet {
        use filter_role::*;
        RoleSet::EMPTY
            .with_if(NAME, a.name != b.name)
            .with_if(FIELDS, a.fields != b.fields)
            .with_if(COLOUR, a.colour != b.colour)
            .with_if(HIGHLIGHT, a.highlight != b.highlight)
            .with_if(NOTIFY, a.notify != b.notify)
            .with_if(NOTIFY_MUTED, a.notify_muted != b.notify_muted)
            .with_if(FIRST, a.first != b.first)
            .with_if(LAST, a.last != b.last)
    }
}

/// The label of a field, as the dialog names it (sentence case).
pub fn field_label(field: Field) -> String {
    match field {
        Field::Text => trc("@label", "Message text").into_owned(),
        Field::Sender => trc("@label", "Sender's name").into_owned(),
        Field::Chat => trc("@label", "Chat's name").into_owned(),
    }
}

/// The fields a spec uses, worded and joined ("Message text, Chat's name").
fn fields_line(spec: &FilterSpec) -> String {
    Field::ALL
        .into_iter()
        .filter(|f| spec.pattern(*f).is_some())
        .map(field_label)
        .collect::<Vec<_>>()
        .join(", ")
}

/// The list's rows from the core's stored filters (every stored row, in the user's order).
pub fn rows(filters: &[Filter]) -> Vec<FilterRow> {
    let text = |p: Option<&str>| p.unwrap_or_default().to_owned();
    filters
        .iter()
        .enumerate()
        .map(|(i, f)| FilterRow {
            id: f.id.0,
            name: f.spec.name.expose().clone(),
            fields: fields_line(&f.spec),
            colour: f.spec.colour,
            highlight: f.spec.highlight,
            notify: f.spec.notify,
            notify_muted: f.spec.notify_muted,
            match_case: f.spec.match_case,
            text: text(f.spec.pattern(Field::Text)),
            sender: text(f.spec.pattern(Field::Sender)),
            chat: text(f.spec.pattern(Field::Chat)),
            first: i == 0,
            last: i + 1 == filters.len(),
        })
        .collect()
}

/// The colour a new filter starts with: the first swatch no filter uses; when all eight are
/// used, the swatches go round again.
pub fn default_colour(used: &[u32]) -> u32 {
    SWATCHES
        .iter()
        .copied()
        .find(|c| !used.contains(c))
        .unwrap_or(SWATCHES[used.len() % SWATCHES.len()])
}

/// A name made from a pattern for "Save as Filter" (R5): the pattern's first characters, one
/// line, at most 24 characters.
pub fn name_from_pattern(pattern: &str) -> String {
    let line: String = pattern
        .lines()
        .next()
        .unwrap_or_default()
        .chars()
        .take(24)
        .collect();
    line.trim().to_owned()
}

/// A short key for a field, what the dialog's inline errors are placed by.
pub fn field_key(field: Field) -> &'static str {
    match field {
        Field::Text => "text",
        Field::Sender => "sender",
        Field::Chat => "chat",
    }
}

/// The sentence for a pattern's syntax error, with its place ("at character %1", counted from
/// 1). Never the pattern.
pub fn error_text(error: &PatternError) -> String {
    let at = (error.at_utf16 + 1).to_string();
    let at = [at.as_str()];
    match error.kind {
        PatternErrorKind::UnclosedGroup => trfc(
            "@info:status",
            "A group is not closed, at character %1.",
            &at,
        ),
        PatternErrorKind::UnclosedClass => trfc(
            "@info:status",
            "A character class is not closed, at character %1.",
            &at,
        ),
        PatternErrorKind::UnknownEscape => trfc(
            "@info:status",
            "This escape sequence is not known, at character %1.",
            &at,
        ),
        PatternErrorKind::NothingToRepeat => trfc(
            "@info:status",
            "There is nothing to repeat here, at character %1.",
            &at,
        ),
        PatternErrorKind::LookAroundUnsupported => trfc(
            "@info:status",
            "Look-around is not supported, at character %1.",
            &at,
        ),
        PatternErrorKind::BackReferenceUnsupported => trfc(
            "@info:status",
            "Back-references are not supported, at character %1.",
            &at,
        ),
        // The compile size is known only at save; no number is promised (the 10,000-character
        // bound is a sanity cap, the compile size refuses much earlier for literal text).
        PatternErrorKind::TooLarge => {
            trc("@info:status", "This pattern is too large or too complex.").into_owned()
        }
        PatternErrorKind::Other => trfc(
            "@info:status",
            "This pattern is not valid, at character %1.",
            &at,
        ),
    }
}

/// The live check of one pattern field: `None` when it is fine or empty (an empty field is not
/// used), else the sentence to show under it.
pub fn check_pattern(pattern: &str) -> Option<String> {
    if pattern.is_empty() {
        return None;
    }
    filters::check(pattern).err().map(|e| error_text(&e))
}

/// What a dialog's Save needs: a name, at least one pattern, and every pattern that is filled
/// parses.
pub fn can_save(name: &str, text: &str, sender: &str, chat: &str) -> bool {
    let name_chars = name.trim().chars().count();
    (1..=filters::MAX_NAME_CHARS).contains(&name_chars)
        && [text, sender, chat].iter().any(|p| !p.is_empty())
        && [text, sender, chat]
            .iter()
            .all(|p| check_pattern(p).is_none())
}

/// What a dialog's inputs make: the spec the core validates again. `..Default::default()` stays
/// although every field is named today: a later switch per filter (the note's "Open for a later
/// consumer") must not break this builder.
#[allow(clippy::too_many_arguments, clippy::needless_update)] // The dialog's fields, one each.
pub fn spec_of(
    name: &str,
    text: &str,
    sender: &str,
    chat: &str,
    match_case: bool,
    notify: bool,
    notify_muted: bool,
    highlight: bool,
    colour: u32,
) -> FilterSpec {
    let opt = |p: &str| (!p.is_empty()).then(|| Redacted::new(p.to_owned()));
    FilterSpec {
        name: Redacted::new(name.to_owned()),
        text_pattern: opt(text),
        sender_pattern: opt(sender),
        chat_pattern: opt(chat),
        match_case,
        // "Also in muted chats" means something only with Notify.
        notify,
        notify_muted: notify && notify_muted,
        highlight,
        colour,
        ..FilterSpec::default()
    }
}

/// A refusal of the core, worded and placed: under which input of the dialog it shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FilterNote {
    /// `"name"`, `"text"`, `"sender"`, `"chat"`, or empty (under no input: the dialog's own
    /// line).
    pub field: &'static str,
    /// The sentence.
    pub text: String,
}

/// The note for a save the core refused.
pub fn refusal_note(refusal: &FilterRefusal) -> FilterNote {
    match refusal {
        FilterRefusal::NameEmpty => FilterNote {
            field: "name",
            text: trc("@info:status", "Give the filter a name.").into_owned(),
        },
        FilterRefusal::NameTooLong => FilterNote {
            field: "name",
            text: trc("@info:status", "This name is too long.").into_owned(),
        },
        FilterRefusal::NoField => FilterNote {
            field: "",
            text: trc("@info:status", "Fill in at least one pattern.").into_owned(),
        },
        FilterRefusal::ColourOutOfRange => FilterNote {
            field: "",
            text: trc("@info:status", "This color cannot be used.").into_owned(),
        },
        FilterRefusal::PatternTooLong(f) => FilterNote {
            field: field_key(*f),
            text: trc("@info:status", "This pattern is too long.").into_owned(),
        },
        FilterRefusal::Pattern(f, e) => FilterNote {
            field: field_key(*f),
            text: error_text(e),
        },
        FilterRefusal::Unknown => FilterNote {
            field: "",
            text: trc("@info:status", "This filter does not exist any more.").into_owned(),
        },
    }
}

/// The note for a save that failed for a reason that is no refusal (storage).
pub fn not_saved_note() -> FilterNote {
    FilterNote {
        field: "",
        text: trc("@info:status", "The filter could not be saved.").into_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kourier_core::facade::FilterId;

    fn filter(id: i64, spec: FilterSpec) -> Filter {
        Filter {
            id: FilterId(id),
            spec,
        }
    }

    #[test]
    fn a_new_filter_takes_the_first_swatch_no_filter_uses() {
        assert_eq!(default_colour(&[]), SWATCHES[0]);
        assert_eq!(default_colour(&[SWATCHES[0], 0x123456]), SWATCHES[1]);
        assert_eq!(default_colour(&[SWATCHES[1]]), SWATCHES[0]);
        assert_eq!(default_colour(&SWATCHES), SWATCHES[0]);
        let nine: Vec<u32> = SWATCHES.iter().copied().chain([1]).collect();
        assert_eq!(default_colour(&nine), SWATCHES[1]);
    }

    #[test]
    fn the_swatches_are_eight_distinct_colours() {
        let mut s = SWATCHES.to_vec();
        s.sort_unstable();
        s.dedup();
        assert_eq!(s.len(), 8);
        assert!(SWATCHES.iter().all(|c| *c <= filters::MAX_COLOUR));
    }

    #[test]
    fn a_pattern_error_is_worded_with_its_place_and_never_quotes_the_pattern() {
        let marker = "zq9marker(";
        let said = check_pattern(marker).expect("an unclosed group");
        assert!(said.contains("group"), "{said}");
        assert!(said.contains("at character 10"), "{said}");
        assert!(!said.contains("zq9marker"), "{said}");
        assert!(check_pattern("").is_none());
        assert!(check_pattern(r"\b\d{3,}\b").is_none());
        for (p, word) in [
            ("[ab", "class"),
            (r"\q", "escape"),
            ("*a", "nothing to repeat"),
            ("(?=a)", "Look-around"),
            (r"(a)\1", "Back-references"),
        ] {
            let said = check_pattern(p).unwrap_or_else(|| panic!("{p} is refused"));
            assert!(said.contains(word), "{p}: {said}");
        }
    }

    #[test]
    fn save_needs_a_name_a_pattern_and_only_valid_patterns() {
        assert!(can_save("Invoices", "invoice", "", ""));
        assert!(can_save("A", "", "", "(?i)work"));
        assert!(!can_save("", "invoice", "", ""), "no name");
        assert!(!can_save("   ", "invoice", "", ""), "a blank name");
        assert!(!can_save("A", "", "", ""), "all fields empty");
        assert!(!can_save("A", "(", "", ""), "an invalid pattern");
        assert!(!can_save("A", "ok", "(", ""), "an invalid second field");
        assert!(!can_save(&"x".repeat(65), "ok", "", ""), "a long name");
        assert!(can_save(&"x".repeat(64), "ok", "", ""));
    }

    #[test]
    fn the_spec_drops_empty_fields_and_muted_needs_notify() {
        let s = spec_of("N", "t", "", "c", true, false, true, true, 5);
        assert_eq!(s.pattern(Field::Text), Some("t"));
        assert_eq!(s.pattern(Field::Sender), None);
        assert_eq!(s.pattern(Field::Chat), Some("c"));
        assert!(s.match_case && s.highlight && !s.notify && !s.notify_muted);
        let s = spec_of("N", "t", "", "", false, true, true, false, 5);
        assert!(s.notify && s.notify_muted);
    }

    #[test]
    fn rows_name_the_fields_and_mark_the_ends() {
        let a = filter(
            1,
            spec_of("One", "x", "", "y", false, true, false, true, SWATCHES[0]),
        );
        let b = filter(
            2,
            spec_of("Two", "", "s", "", false, false, false, false, SWATCHES[1]),
        );
        let r = rows(&[a, b]);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].fields, "Message text, Chat's name");
        assert_eq!(r[1].fields, "Sender's name");
        assert!(r[0].first && !r[0].last && !r[1].first && r[1].last);
        assert_eq!((r[0].text.as_str(), r[0].sender.as_str()), ("x", ""));
        let shown = format!("{:?}", r[0]);
        assert!(!shown.contains("One") && !shown.contains('x'), "{shown}");
    }

    #[test]
    fn every_refusal_is_worded_under_its_own_field() {
        let e = PatternError {
            kind: PatternErrorKind::UnclosedGroup,
            at_utf16: 2,
        };
        let n = refusal_note(&FilterRefusal::Pattern(Field::Sender, e));
        assert_eq!(n.field, "sender");
        assert!(n.text.contains("at character 3"), "{}", n.text);
        let big = PatternError {
            kind: PatternErrorKind::TooLarge,
            at_utf16: 0,
        };
        let n = refusal_note(&FilterRefusal::Pattern(Field::Chat, big));
        assert_eq!(n.field, "chat");
        assert!(!n.text.contains("10,000") && !n.text.contains("character"));
        assert_eq!(refusal_note(&FilterRefusal::NameEmpty).field, "name");
        assert_eq!(refusal_note(&FilterRefusal::NameTooLong).field, "name");
        assert_eq!(refusal_note(&FilterRefusal::NoField).field, "");
        assert_eq!(
            refusal_note(&FilterRefusal::PatternTooLong(Field::Text)).field,
            "text"
        );
        assert_eq!(refusal_note(&FilterRefusal::ColourOutOfRange).field, "");
        assert_eq!(refusal_note(&FilterRefusal::Unknown).field, "");
    }

    #[test]
    fn a_name_from_a_pattern_is_one_short_line() {
        assert_eq!(name_from_pattern("invoice\nsecond"), "invoice");
        assert_eq!(name_from_pattern(&"a".repeat(40)).chars().count(), 24);
        assert_eq!(name_from_pattern(""), "");
    }
}
