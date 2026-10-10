//! Editing a message that has formatting or mentions (§3.6) in a composer that has neither.
//!
//! The composer edits plain text: the message's body with each mention written out as
//! "@Name" ([`EditForm::text`]). What the user changed is taken to be one stretch of it,
//! everything between the longest common beginning and end of the old and the new text; the
//! rest of the message is as it was and keeps its formatting ranges and its mentions
//! ([`EditForm::apply`]). A formatting range the change lies inside grows or shrinks with it
//! (a typo fixed inside a spoiler stays hidden), text typed right at its edge without a space
//! joins it (a digit added to a hidden code), one the change cuts into keeps its untouched
//! part, and a mention or a link whose own text was changed becomes plain text. Without this
//! an edit would send the bare text: a spoiler would be revealed and every mention lost.

use std::collections::HashMap;

use kourier_model::text::validate_range;
use kourier_model::{RangeKind, Text, TextRange};

use crate::names::Names;

/// One mention: where its "@Name" is in the edited text and where its placeholder is in the
/// body (byte offsets).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Mention {
    shown: usize,
    shown_len: usize,
    body_len: usize,
}

/// A message text as the composer edits it.
#[derive(Clone, PartialEq, Eq)]
pub struct EditForm {
    text: String,
    /// In text order.
    mentions: Vec<Mention>,
}

crate::safe_debug!(EditForm { mentions });

fn is_mention(kind: &RangeKind) -> bool {
    matches!(kind, RangeKind::Mention(_) | RangeKind::MentionAll)
}

/// Length in bytes of the longest common beginning of `a` and `b` (whole characters).
fn common_prefix(a: &str, b: &str) -> usize {
    a.char_indices()
        .zip(b.chars())
        .find(|((_, x), y)| x != y)
        .map_or_else(|| a.len().min(b.len()), |((at, _), _)| at)
}

/// Length in bytes of the longest common end of `a` and `b` (whole characters).
fn common_suffix(a: &str, b: &str) -> usize {
    a.chars()
        .rev()
        .zip(b.chars().rev())
        .take_while(|(x, y)| x == y)
        .map(|(x, _)| x.len_utf8())
        .sum()
}

impl EditForm {
    /// The editable form of `text`: its body, each mention as "@Name" (the name the timeline
    /// shows).
    pub fn new(text: &Text, names: &dyn Names) -> Self {
        let body = text.body.as_str();
        let at_mention: HashMap<usize, &RangeKind> = text
            .ranges
            .iter()
            .filter(|r| is_mention(&r.kind) && validate_range(body, r).is_ok())
            .map(|r| (r.start as usize, &r.kind))
            .collect();
        let mut shown = String::with_capacity(body.len());
        let mut mentions = Vec::new();
        for (at, c) in body.char_indices() {
            let Some(kind) = at_mention.get(&at) else {
                shown.push(c);
                continue;
            };
            let name = match kind {
                RangeKind::Mention(user) => names.display_name(user),
                _ => "everyone".to_owned(),
            };
            let start = shown.len();
            shown.push('@');
            shown.extend(name.chars().map(|c| if c.is_control() { ' ' } else { c }));
            mentions.push(Mention {
                shown: start,
                shown_len: shown.len() - start,
                body_len: c.len_utf8(),
            });
        }
        Self {
            text: shown,
            mentions,
        }
    }

    /// What the composer shows and edits.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The body offset of the offset `shown` of [`Self::text`] (not inside a mention's name).
    fn to_body(&self, shown: usize) -> usize {
        self.mentions
            .iter()
            .take_while(|m| m.shown + m.shown_len <= shown)
            .fold(shown, |at, m| at - m.shown_len + m.body_len)
    }

    /// `original` (the text this form was made of) with the user's change applied: `new` is
    /// the composer's text when the edit was saved. Formatting and mentions outside the
    /// changed stretch are kept, trailing white space is trimmed.
    pub fn apply(&self, original: &Text, new: &str) -> Text {
        let old = self.text.as_str();
        let mut prefix = common_prefix(old, new);
        let suffix = common_suffix(&old[prefix..], &new[prefix..]);
        // Where the unchanged end starts in the old text.
        let mut tail = old.len() - suffix;
        // A mention is one piece: a change inside its name replaces all of it.
        for m in &self.mentions {
            let (start, end) = (m.shown, m.shown + m.shown_len);
            if start < prefix && prefix < end {
                prefix = start;
            }
            if start < tail && tail < end {
                tail = end;
            }
        }
        let suffix = old.len() - tail;
        let body = original.body.as_str();
        let (from, to) = (self.to_body(prefix), self.to_body(tail));
        let (Some(head), Some(end), Some(mid)) = (
            body.get(..from),
            body.get(to..),
            new.get(prefix..new.len().saturating_sub(suffix)),
        ) else {
            // Not the text this form was made of: nothing of it can be kept.
            return Text::plain(new.trim_end());
        };
        let mut out = String::with_capacity(head.len() + mid.len() + end.len());
        out.push_str(head);
        out.push_str(mid);
        out.push_str(end);
        out.truncate(out.trim_end().len());
        let ranges = original
            .ranges
            .iter()
            .filter(|r| validate_range(body, r).is_ok())
            .filter_map(|r| remap(r, from, to, mid, out.len()))
            .filter(|r| validate_range(&out, r).is_ok())
            .collect();
        Text { body: out, ranges }
    }
}

/// Where `range` of the old body is in the new one: the bytes `from..to` of the old body
/// were replaced by `mid`, and the new body is `len` bytes long. `None` when nothing is left
/// of it.
fn remap(range: &TextRange, from: usize, to: usize, mid: &str, len: usize) -> Option<TextRange> {
    let start = range.start as usize;
    let end = start + range.len as usize;
    // A mention and a link stand for something: they neither grow nor survive a change.
    let whole = is_mention(&range.kind) || matches!(range.kind, RangeKind::Link(_));
    // An offset at or after the changed stretch, in the new body.
    let moved = |at: usize| at - to + from + mid.len();
    let (start, end) = if end <= from {
        // Before the change. What was typed right behind it without a space continues it,
        // as in any editor: a digit added to a hidden code is hidden too.
        let joined = if end == from && !whole {
            mid.len() - mid.trim_start_matches(|c: char| !c.is_whitespace()).len()
        } else {
            0
        };
        (start, end + joined)
    } else if start >= to {
        // After the change; what was typed right in front of it without a space joins it.
        let joined = if start == to && !whole {
            mid.len() - mid.trim_end_matches(|c: char| !c.is_whitespace()).len()
        } else {
            0
        };
        (moved(start) - joined, moved(end))
    } else if whole {
        // Its own text was changed: it is what the user typed now.
        return None;
    } else if start <= from && end >= to {
        // The change lies inside it: it grows or shrinks with the change.
        (start, moved(end))
    } else if start < from {
        // Its end was changed: the part before the change stays.
        (start, from)
    } else if end > to {
        // Its beginning was changed: the part after the change stays.
        (from + mid.len(), moved(end))
    } else {
        // It was inside what the user replaced.
        return None;
    };
    let end = end.min(len);
    if start >= end {
        return None;
    }
    Some(TextRange {
        start: u32::try_from(start).ok()?,
        len: u32::try_from(end - start).ok()?,
        kind: range.kind.clone(),
    })
}

#[cfg(test)]
mod tests {
    use kourier_model::UserId;
    use kourier_model::text::MENTION_PLACEHOLDER;

    use super::*;
    use crate::names::NameMap;

    fn range(start: usize, len: usize, kind: RangeKind) -> TextRange {
        TextRange {
            start: start as u32,
            len: len as u32,
            kind,
        }
    }

    fn olena() -> UserId {
        UserId::parse("aci:00000000-0000-4000-8000-000000000001").expect("id")
    }

    fn names() -> NameMap {
        let mut names = NameMap::new();
        names.insert(olena(), "Olena");
        names
    }

    /// The parts of `text` covered by ranges of `kind`.
    fn covered<'a>(text: &'a Text, kind: &RangeKind) -> Vec<&'a str> {
        text.ranges
            .iter()
            .filter(|r| &r.kind == kind)
            .map(|r| &text.body[r.start as usize..(r.start + r.len) as usize])
            .collect()
    }

    fn edit(original: &Text, new: &str) -> Text {
        let n = names();
        let form = EditForm::new(original, &n);
        let edited = form.apply(original, new);
        for r in &edited.ranges {
            validate_range(&edited.body, r).expect("a valid range");
        }
        edited
    }

    #[test]
    fn plain_text_is_simply_replaced() {
        let original = Text::plain("Dinner at seven");
        let n = names();
        assert_eq!(EditForm::new(&original, &n).text(), "Dinner at seven");
        assert_eq!(
            edit(&original, "Dinner at eight  \n"),
            Text::plain("Dinner at eight")
        );
        assert_eq!(edit(&original, "Dinner at seven"), original);
        assert_eq!(edit(&original, "Lunch"), Text::plain("Lunch"));
        assert_eq!(edit(&original, ""), Text::plain(""));
    }

    #[test]
    fn a_typo_fixed_inside_a_spoiler_stays_hidden() {
        // "The killer is [the butlre], sadly" with the bracketed part hidden.
        let body = "The killer is the butlre, sadly";
        let original = Text {
            body: body.into(),
            ranges: vec![
                range(14, 10, RangeKind::Spoiler),
                range(0, 3, RangeKind::Bold),
                range(26, 5, RangeKind::Italic),
            ],
        };
        assert_eq!(covered(&original, &RangeKind::Spoiler), ["the butlre"]);
        let edited = edit(&original, "The killer is the butler, sadly");
        assert_eq!(edited.body, "The killer is the butler, sadly");
        assert_eq!(covered(&edited, &RangeKind::Spoiler), ["the butler"]);
        // What comes before and after the change is as it was.
        assert_eq!(covered(&edited, &RangeKind::Bold), ["The"]);
        assert_eq!(covered(&edited, &RangeKind::Italic), ["sadly"]);

        // The hidden part gets longer, and what follows moves with it.
        let edited = edit(&original, "The killer is the old butler, sadly");
        assert_eq!(covered(&edited, &RangeKind::Spoiler), ["the old butler"]);
        assert_eq!(covered(&edited, &RangeKind::Italic), ["sadly"]);
        // Retyped as a whole, it is still the hidden part.
        let edited = edit(&original, "The killer is the gardener, sadly");
        assert_eq!(covered(&edited, &RangeKind::Spoiler), ["the gardener"]);
        // Words added outside it are not hidden.
        let edited = edit(&original, "The killer is surely the butlre, sadly");
        assert_eq!(covered(&edited, &RangeKind::Spoiler), ["the butlre"]);
        let edited = edit(&original, "The killer is the butlre for sure, sadly");
        assert_eq!(covered(&edited, &RangeKind::Spoiler), ["the butlre"]);
    }

    #[test]
    fn text_typed_at_the_edge_of_a_range_without_a_space_joins_it() {
        // "PIN [1234] ok" with the code hidden and "ok" bold.
        let original = Text {
            body: "PIN 1234 ok".into(),
            ranges: vec![
                range(4, 4, RangeKind::Spoiler),
                range(9, 2, RangeKind::Bold),
            ],
        };
        // A digit behind the code, a digit in front of it: both are part of what is hidden.
        let edited = edit(&original, "PIN 12345 ok");
        assert_eq!(covered(&edited, &RangeKind::Spoiler), ["12345"]);
        assert_eq!(covered(&edited, &RangeKind::Bold), ["ok"]);
        let edited = edit(&original, "PIN 01234 ok");
        assert_eq!(covered(&edited, &RangeKind::Spoiler), ["01234"]);
        // Only up to the next space: a new word is not hidden.
        let edited = edit(&original, "PIN 12345 or so ok");
        assert_eq!(covered(&edited, &RangeKind::Spoiler), ["12345"]);
        let edited = edit(&original, "PIN 1234 and ok");
        assert_eq!(covered(&edited, &RangeKind::Spoiler), ["1234"]);
        assert_eq!(covered(&edited, &RangeKind::Bold), ["ok"]);
        let edited = edit(&original, "PIN is 1234 ok");
        assert_eq!(covered(&edited, &RangeKind::Spoiler), ["1234"]);
        // A word continued at the end of the text keeps its style.
        let edited = edit(&original, "PIN 1234 okay");
        assert_eq!(covered(&edited, &RangeKind::Bold), ["okay"]);
    }

    #[test]
    fn a_change_across_a_range_keeps_its_untouched_part() {
        // Bold "seven sharp"; the change replaces "sharp tonight" (the end of the range and
        // the text after it).
        let original = Text {
            body: "Dinner at seven sharp tonight, all".into(),
            ranges: vec![range(10, 11, RangeKind::Bold)],
        };
        assert_eq!(covered(&original, &RangeKind::Bold), ["seven sharp"]);
        let edited = edit(&original, "Dinner at seven or so, all");
        assert_eq!(covered(&edited, &RangeKind::Bold), ["seven "]);
        // The beginning of the range is replaced with the text before it.
        let edited = edit(&original, "Lunch from eleven sharp tonight, all");
        assert_eq!(edited.body, "Lunch from eleven sharp tonight, all");
        assert_eq!(covered(&edited, &RangeKind::Bold), ["even sharp"]);
        // The whole range is replaced with more around it: nothing of it is left.
        let edited = edit(&original, "Dinner is off, all");
        assert!(edited.ranges.is_empty(), "{edited:?}");
        // Deleted to the end: the range ends with the text.
        let edited = edit(&original, "Dinner at seven");
        assert_eq!(covered(&edited, &RangeKind::Bold), ["seven"]);
    }

    #[test]
    fn mentions_stay_mentions_unless_their_name_is_changed() {
        let p = MENTION_PLACEHOLDER;
        let body = format!("Hi {p}, see {p} too. Ой, {p}!");
        let first = body.find(p).expect("placeholder");
        let second = first + p.len_utf8() + ", see ".len();
        let third = body.rfind(p).expect("placeholder");
        let original = Text {
            body: body.clone(),
            ranges: vec![
                range(first, 3, RangeKind::Mention(olena())),
                range(second, 3, RangeKind::MentionAll),
                range(third, 3, RangeKind::Mention(olena())),
                range(0, 2, RangeKind::Bold),
            ],
        };
        let n = names();
        let form = EditForm::new(&original, &n);
        assert_eq!(form.text(), "Hi @Olena, see @everyone too. Ой, @Olena!");
        // Nothing changed: the very same text, placeholders and all.
        assert_eq!(form.apply(&original, form.text()), original);

        // A word added between the mentions (multi-byte text around): all three stay.
        let edited = edit(
            &original,
            "Hi @Olena, please see @everyone too. Ой, @Olena!",
        );
        assert_eq!(edited.body, format!("Hi {p}, please see {p} too. Ой, {p}!"));
        assert_eq!(
            edited.ranges.iter().filter(|r| is_mention(&r.kind)).count(),
            3
        );
        assert_eq!(covered(&edited, &RangeKind::Bold), ["Hi"]);

        // The second mention's name is changed: it is plain text now, the others stay.
        let edited = edit(&original, "Hi @Olena, see @everybody too. Ой, @Olena!");
        assert_eq!(edited.body, format!("Hi {p}, see @everybody too. Ой, {p}!"));
        assert_eq!(covered(&edited, &RangeKind::Mention(olena())).len(), 2);
        assert!(covered(&edited, &RangeKind::MentionAll).is_empty());

        // A mention deleted: its placeholder goes with it.
        let edited = edit(&original, "Hi, see @everyone too. Ой, @Olena!");
        assert_eq!(edited.body, format!("Hi, see {p} too. Ой, {p}!"));
        assert_eq!(covered(&edited, &RangeKind::Mention(olena())).len(), 1);
        assert_eq!(covered(&edited, &RangeKind::MentionAll).len(), 1);

        // Text typed right after a mention does not touch it.
        let edited = edit(&original, "Hi @Olena!, see @everyone too. Ой, @Olena!");
        assert_eq!(edited.body, format!("Hi {p}!, see {p} too. Ой, {p}!"));
        assert_eq!(covered(&edited, &RangeKind::Mention(olena())).len(), 2);

        // Everything replaced: plain text, no stray placeholder.
        let edited = edit(&original, "Never mind");
        assert_eq!(edited, Text::plain("Never mind"));
    }

    #[test]
    fn a_link_whose_text_changed_is_no_link_any_more() {
        let original = Text {
            body: "See the plan here".into(),
            ranges: vec![range(
                8,
                4,
                RangeKind::Link("https://example.org/plan".into()),
            )],
        };
        let edited = edit(&original, "See the pl-an here");
        assert_eq!(edited.body, "See the pl-an here");
        assert!(edited.ranges.is_empty());
        // Changed elsewhere (also right behind it), the link stays on its word.
        let edited = edit(&original, "See the plan, here");
        assert_eq!(
            covered(&edited, &RangeKind::Link("https://example.org/plan".into())),
            ["plan"]
        );
        let edited = edit(&original, "Look: the plan here");
        assert_eq!(
            covered(&edited, &RangeKind::Link("https://example.org/plan".into())),
            ["plan"]
        );
    }

    #[test]
    fn broken_ranges_and_a_foreign_text_do_no_harm() {
        // Ranges that do not fit their body are dropped, not carried along.
        let original = Text {
            body: "héllo wörld".into(),
            ranges: vec![
                range(2, 3, RangeKind::Bold),
                range(40, 3, RangeKind::Italic),
                range(0, 0, RangeKind::Mono),
                range(7, 6, RangeKind::Strike),
            ],
        };
        let edited = edit(&original, "héllo, wörld");
        assert_eq!(edited.body, "héllo, wörld");
        assert_eq!(covered(&edited, &RangeKind::Strike), ["wörld"]);
        assert_eq!(edited.ranges.len(), 1);
        // A form applied to another text than its own gives the new text, nothing else.
        let n = names();
        let form = EditForm::new(&Text::plain("a much longer text than the other"), &n);
        let other = Text {
            body: "short".into(),
            ranges: vec![range(0, 5, RangeKind::Bold)],
        };
        let edited = form.apply(&other, "a much longer text than the other one");
        for r in &edited.ranges {
            validate_range(&edited.body, r).expect("a valid range");
        }
    }
}
