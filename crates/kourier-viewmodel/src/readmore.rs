//! "Read more" for long message texts (architecture §13, phase M): which part of a text the
//! timeline shows, as Signal Desktop does (`MessageBodyReadMore`, `graphemeAndLinkAwareSlice`).
//!
//! A text shows its first [`INITIAL`] units; each "Read more" adds [`INCREMENT`]. When no more
//! than [`BUFFER`] units would be left, all of it is shown. Units are grapheme clusters, not
//! bytes or UTF-16 units (Desktop counts UTF-16 units and cuts at the next grapheme boundary):
//! a flag or a family of emoji is one unit and never split.
//!
//! The cut is made on the text and its ranges, before the RichText is built, never on HTML: the
//! shown part is a [`Text`] of its own, so its markup is well formed and looks as the same
//! part of the whole text does. A formatting range (bold, italic, strike, monospace, spoiler)
//! that crosses the cut ends at it; a link and a mention are not cut: the cut moves to the end
//! of the one it falls into.

use std::cell::RefCell;
use std::collections::HashMap;

use kourier_model::text::validate_range;
use kourier_model::{MessageId, RangeKind, Text, TextRange};
use unicode_segmentation::{GraphemeCursor, UnicodeSegmentation};

use crate::names::Names;
use crate::richtext::{self, RichText};

/// Units shown before the first "Read more" (Desktop: `INITIAL_LENGTH`).
pub const INITIAL: usize = 800;
/// Units each "Read more" adds (Desktop: `INCREMENT_COUNT`).
pub const INCREMENT: usize = 3000;
/// A text with no more than this many units beyond the limit is shown whole (Desktop:
/// `BUFFER`).
pub const BUFFER: usize = 100;

/// Where `text` is cut when `limit` units are shown: a byte offset into its body, or `None`
/// when all of it is shown (it has at most `limit + BUFFER` units, or the cut moved to its
/// end).
///
/// The work is bounded by the part that is shown: units are counted up to `limit + BUFFER + 1`
/// and no further.
pub fn cut_at(text: &Text, limit: usize) -> Option<usize> {
    // What is displayed: trailing whitespace is dropped.
    let body = text.body.trim_end();
    // A unit is at least one byte.
    if body.len() <= limit.saturating_add(BUFFER) {
        return None;
    }
    let mut units = body.grapheme_indices(true);
    // The first unit beyond the limit starts the rest; more than BUFFER units must follow
    // the limit for a cut to be worth it.
    let (mut cut, _) = units.nth(limit)?;
    units.nth(BUFFER - 1)?;

    let spans = unbreakable(text, body);
    // Moving the cut can land in another span or in a unit: until it rests.
    loop {
        let mut next = cut;
        if let Some(&(_, end)) = spans.iter().find(|&&(s, e)| s < next && next < e) {
            next = end;
        }
        next = unit_end(body, next);
        if next == cut {
            break;
        }
        cut = next;
    }
    (cut < body.len()).then_some(cut)
}

/// Byte ranges of `body` the cut must not fall inside: links (bare URLs as the message shows
/// them, and the text's own link ranges) and mentions.
fn unbreakable(text: &Text, body: &str) -> Vec<(usize, usize)> {
    let mut spans = richtext::find_urls(body);
    for r in &text.ranges {
        if matches!(
            r.kind,
            RangeKind::Link(_) | RangeKind::Mention(_) | RangeKind::MentionAll
        ) && validate_range(&text.body, r).is_ok()
        {
            let s = r.start as usize;
            spans.push((s, s + r.len as usize));
        }
    }
    spans
}

/// `at`, or the end of the grapheme cluster it falls into.
fn unit_end(body: &str, at: usize) -> usize {
    let mut cursor = GraphemeCursor::new(at, body.len(), true);
    if cursor.is_boundary(body, 0).unwrap_or(true) {
        return at;
    }
    cursor
        .next_boundary(body, 0)
        .ok()
        .flatten()
        .unwrap_or(body.len())
}

/// The part of `text` before byte `cut`: ranges that cross the cut end at it, ranges after it
/// go. (Ranges that are not valid for the whole text are dropped first, so that cutting
/// cannot make one valid.)
pub fn truncate(text: &Text, cut: usize) -> Text {
    let ranges = text
        .ranges
        .iter()
        .filter(|r| validate_range(&text.body, r).is_ok())
        .filter_map(|r| {
            let start = r.start as usize;
            let end = start + r.len as usize;
            (start < cut).then(|| TextRange {
                start: r.start,
                len: u32::try_from(end.min(cut) - start).unwrap_or(r.len),
                kind: r.kind.clone(),
            })
        })
        .collect();
    Text {
        body: text.body[..cut].to_owned(),
        ranges,
    }
}

/// `text` as it is shown with `limit` units: the cut part with [`RichText::more`] set, or all
/// of it.
pub fn render_limited(text: &Text, names: &dyn Names, limit: usize) -> RichText {
    let Some(cut) = cut_at(text, limit) else {
        return richtext::render(text, names);
    };
    let mut rich = richtext::render(&truncate(text, cut), names);
    rich.more = true;
    // How much is left, for the hint in "Read more". Counted the simple way (user decision,
    // 2026-10-08): what stands between spaces and line breaks is a word.
    let rest = text.body.trim_end().get(cut..).unwrap_or_default();
    rich.more_words = u32::try_from(rest.split_whitespace().count()).unwrap_or(u32::MAX);
    // The cut can fall inside a word (as Desktop's does): an ellipsis says that the text goes
    // on. Only in the markup: `plain` stays a prefix of the whole text, which the spoiler
    // offsets and the tests count on.
    rich.html.push('…');
    // (A part of a long text is no "big emoji" message, whatever it is made of.)
    rich.jumbo = false;
    rich.whole = richtext::render(text, names).plain;
    rich
}

/// How much of each long text the open conversation shows, and the texts cut for it.
///
/// The limits live as long as the conversation is open ([`Self::clear`] when another one is).
/// A cut text is kept with the row it was made for and made again only when its text, its
/// limit or the names of the people it mentions change, so the timeline's frequent rebuilds do
/// not format long texts again.
#[derive(Default)]
pub struct ReadMore {
    /// Limits raised by "Read more", per message; a message without one shows [`INITIAL`].
    limits: HashMap<MessageId, usize>,
    cache: RefCell<Cache>,
}

#[derive(Default)]
struct Cache {
    entries: HashMap<MessageId, Cached>,
    /// Counts the passes over the rows ([`ReadMore::begin`]).
    pass: u64,
}

struct Cached {
    /// The pass that last used it.
    pass: u64,
    text: Text,
    limit: usize,
    /// The names the mentions of `text` had.
    mentions: Vec<String>,
    rich: RichText,
}

impl ReadMore {
    /// Units shown of message `id`.
    pub fn limit(&self, id: MessageId) -> usize {
        self.limits.get(&id).copied().unwrap_or(INITIAL)
    }

    /// One "Read more" on message `id`.
    pub fn raise(&mut self, id: MessageId) {
        let limit = self.limit(id).saturating_add(INCREMENT);
        self.limits.insert(id, limit);
    }

    /// Forgets everything (another conversation is open).
    pub fn clear(&mut self) {
        self.limits.clear();
        *self.cache.get_mut() = Cache::default();
    }

    /// A pass over the rows begins: [`Self::render`] calls follow, then [`Self::end`].
    pub fn begin(&self) {
        self.cache.borrow_mut().pass += 1;
    }

    /// The pass is over: texts it did not ask for are forgotten (their messages left the
    /// window).
    pub fn end(&self) {
        let mut cache = self.cache.borrow_mut();
        let pass = cache.pass;
        cache.entries.retain(|_, c| c.pass == pass);
    }

    /// The text of message `id` as the timeline shows it now.
    pub fn render(&self, id: MessageId, text: &Text, names: &dyn Names) -> RichText {
        // Short texts are never cut and cost nothing to format.
        if text.body.len() <= INITIAL {
            return richtext::render(text, names);
        }
        let limit = self.limit(id);
        let mentions: Vec<String> = text
            .ranges
            .iter()
            .filter_map(|r| match &r.kind {
                RangeKind::Mention(user) => Some(names.display_name(user)),
                _ => None,
            })
            .collect();
        let mut cache = self.cache.borrow_mut();
        let pass = cache.pass;
        if let Some(c) = cache.entries.get_mut(&id)
            && c.limit == limit
            && c.text == *text
            && c.mentions == mentions
        {
            c.pass = pass;
            return c.rich.clone();
        }
        let rich = render_limited(text, names, limit);
        cache.entries.insert(
            id,
            Cached {
                pass,
                text: text.clone(),
                limit,
                mentions,
                rich: rich.clone(),
            },
        );
        rich
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kourier_model::UserId;

    use crate::richtext::Span16;

    struct TestNames;
    impl Names for TestNames {
        fn display_name(&self, _user: &UserId) -> String {
            "Olena".into()
        }
    }

    fn range(start: usize, len: usize, kind: RangeKind) -> TextRange {
        TextRange {
            start: u32::try_from(start).expect("start"),
            len: u32::try_from(len).expect("len"),
            kind,
        }
    }

    fn text(body: String, ranges: Vec<TextRange>) -> Text {
        Text { body, ranges }
    }

    /// `t` as shown with `limit` units. A cut text's markup ends with the ellipsis; it is
    /// taken off here, so the tests below compare the markup of the text itself.
    fn shown(t: &Text, limit: usize) -> RichText {
        let mut rich = render_limited(t, &TestNames, limit);
        assert_eq!(rich.html.ends_with('…'), rich.more, "{}", rich.html);
        if rich.more {
            rich.html.pop();
        }
        rich
    }

    /// Every opened tag is closed again, in order.
    fn assert_balanced(html: &str) {
        let mut open: Vec<&str> = Vec::new();
        let mut rest = html;
        while let Some(i) = rest.find('<') {
            let after = &rest[i + 1..];
            let close = after.find('>').expect("a tag ends");
            let tag = &after[..close];
            if let Some(name) = tag.strip_prefix('/') {
                assert_eq!(open.pop(), Some(name), "closing {tag} in {html}");
            } else if tag != "br/" {
                open.push(tag.split(' ').next().unwrap_or(tag));
            }
            rest = &after[close + 1..];
        }
        assert!(open.is_empty(), "left open: {open:?} in {html}");
    }

    #[test]
    fn eight_hundred_and_up_to_nine_hundred_units_are_shown_whole() {
        for n in [1, 799, 800, 801, 850, 900] {
            let t = text("a".repeat(n), vec![]);
            assert_eq!(cut_at(&t, INITIAL), None, "{n}");
            let r = shown(&t, INITIAL);
            assert!(!r.more, "{n}");
            assert_eq!(r.plain.len(), n);
        }
    }

    /// The hint of "Read more": the words of the part that is not shown, counted by spaces
    /// and line breaks; a text shown whole has none.
    #[test]
    fn the_hidden_part_is_counted_in_words() {
        // 800 units shown: 160 times "word ". Hidden: 40 more of them and three odd ones.
        let body = format!("{}one\ntwo  three \n", "word ".repeat(200));
        let r = shown(&text(body, vec![]), INITIAL);
        assert!(r.more);
        assert_eq!(r.more_words, 43);
        // One step further everything shows.
        let t = text("word ".repeat(200), vec![]);
        let r = shown(&t, INITIAL + INCREMENT);
        assert!(!r.more);
        assert_eq!(r.more_words, 0);
        // A cut inside a word: its rest counts as a word.
        let r = shown(&text("a".repeat(901), vec![]), INITIAL);
        assert_eq!(r.more_words, 1);
    }

    #[test]
    fn nine_hundred_and_one_units_are_cut_at_eight_hundred() {
        let t = text("a".repeat(901), vec![]);
        assert_eq!(cut_at(&t, INITIAL), Some(800));
        let r = shown(&t, INITIAL);
        assert!(r.more);
        assert_eq!(r.plain, "a".repeat(800));
        assert_eq!(r.html, "a".repeat(800));
        assert_eq!(r.whole, "a".repeat(901));
        assert_eq!(r.full_plain(), "a".repeat(901));
        // A text that is shown whole has no separate whole.
        let r = shown(&text("a".repeat(900), vec![]), INITIAL);
        assert_eq!(r.full_plain(), "a".repeat(900));
        assert!(r.whole.is_empty());
    }

    #[test]
    fn trailing_whitespace_does_not_make_a_text_long() {
        let t = text(format!("{}{}", "a".repeat(850), " \n".repeat(200)), vec![]);
        assert_eq!(cut_at(&t, INITIAL), None);
    }

    #[test]
    fn units_are_grapheme_clusters() {
        // Characters are 4 bytes and two code points each; a unit is one flag.
        let t = text("🇺🇦".repeat(900), vec![]);
        assert_eq!(cut_at(&t, INITIAL), None, "900 flags are 900 units");
        let t = text("🇺🇦".repeat(901), vec![]);
        assert_eq!(cut_at(&t, INITIAL), Some(800 * 8));
    }

    #[test]
    fn a_flag_is_not_cut_in_two() {
        // The 800th unit is a flag: counting code points would stop between its halves.
        let t = text(format!("{}🇺🇦{}", "a".repeat(799), "b".repeat(200)), vec![]);
        let r = shown(&t, INITIAL);
        assert!(r.more);
        assert_eq!(r.plain, format!("{}🇺🇦", "a".repeat(799)));
        // And a flag after 798 units, then three more characters: the pair stays whole.
        let t = text(
            format!("{}🇺🇦🇺🇦🇺🇦{}", "a".repeat(798), "b".repeat(200)),
            vec![],
        );
        assert_eq!(shown(&t, INITIAL).plain, format!("{}🇺🇦🇺🇦", "a".repeat(798)));
    }

    #[test]
    fn a_zwj_family_is_not_cut_in_pieces() {
        let family = "👨\u{200D}👩\u{200D}👧";
        let t = text(
            format!("{}{family}{}", "a".repeat(799), "b".repeat(200)),
            vec![],
        );
        let r = shown(&t, INITIAL);
        assert!(r.more);
        assert_eq!(r.plain, format!("{}{family}", "a".repeat(799)));
        // With the cut limit one unit earlier, the family is left out whole.
        let r = shown(&t, 799);
        assert_eq!(r.plain, "a".repeat(799));
    }

    #[test]
    fn a_bare_link_is_shown_to_its_end() {
        let url = format!("https://example.org/{}", "p".repeat(30));
        let body = format!("{} {url} tail {}", "x".repeat(790), "y".repeat(200));
        let t = text(body, vec![]);
        let r = shown(&t, INITIAL);
        assert!(r.more);
        assert!(r.plain.ends_with(&url), "{}", &r.plain[780..]);
        assert!(
            r.html.ends_with(&format!("<a href=\"{url}\">{url}</a>")),
            "{}",
            r.html
        );
        assert_balanced(&r.html);
    }

    #[test]
    fn a_link_range_is_shown_to_its_end() {
        let label = "z".repeat(40);
        let body = format!("{} {label} tail {}", "x".repeat(790), "y".repeat(200));
        let t = text(
            body,
            vec![range(
                791,
                40,
                RangeKind::Link("https://example.org/".into()),
            )],
        );
        let r = shown(&t, INITIAL);
        assert!(r.more);
        assert!(r.plain.ends_with(&label));
        assert!(
            r.html
                .ends_with(&format!("<a href=\"https://example.org/\">{label}</a>"))
        );
        assert_balanced(&r.html);
        // A link that runs to the end of the text leaves nothing to read more of.
        let body = format!("{} {}", "x".repeat(790), "z".repeat(300));
        let t = text(
            body,
            vec![range(
                791,
                300,
                RangeKind::Link("https://example.org/".into()),
            )],
        );
        assert_eq!(cut_at(&t, INITIAL), None);
    }

    #[test]
    fn a_mention_is_shown_whole_or_not_at_all() {
        use kourier_model::text::MENTION_PLACEHOLDER as M;
        let olena = UserId::parse("aci:olena").expect("id");
        let mention = |at: usize| range(at, M.len_utf8(), RangeKind::Mention(olena.clone()));
        // The 800th unit is the mention.
        let t = text(
            format!("{}{M}{}", "m".repeat(799), "z".repeat(200)),
            vec![mention(799)],
        );
        let r = shown(&t, INITIAL);
        assert!(r.more);
        assert_eq!(r.plain, format!("{}@Olena", "m".repeat(799)));
        assert!(r.html.ends_with("<b>@Olena</b>"), "{}", r.html);
        assert_eq!(
            r.whole,
            format!("{}@Olena{}", "m".repeat(799), "z".repeat(200))
        );
        // The 801st: the cut is right before it and it waits for the next step.
        let t = text(
            format!("{}{M}{}", "m".repeat(800), "z".repeat(200)),
            vec![mention(800)],
        );
        let r = shown(&t, INITIAL);
        assert!(r.more);
        assert_eq!(r.plain, "m".repeat(800));
    }

    #[test]
    fn formatting_that_crosses_the_cut_ends_at_it() {
        let t = text(
            "b".repeat(1000),
            vec![
                range(700, 200, RangeKind::Bold),
                range(750, 250, RangeKind::Italic),
                range(900, 50, RangeKind::Mono),
            ],
        );
        let r = shown(&t, INITIAL);
        assert!(r.more);
        assert_eq!(
            r.html,
            format!(
                "{}<b>{}<i>{}</i></b>",
                "b".repeat(700),
                "b".repeat(50),
                "b".repeat(50)
            )
        );
        assert_balanced(&r.html);
        assert_eq!(r.plain.len(), 800);
        // The same part of the whole text looks the same.
        let full = richtext::render(&t, &TestNames);
        assert!(
            full.html
                .starts_with(&r.html[..r.html.len() - "</i></b>".len()])
        );
    }

    #[test]
    fn a_spoiler_that_crosses_the_cut_ends_at_it() {
        let t = text(
            "s".repeat(1000),
            vec![
                range(790, 100, RangeKind::Spoiler),
                range(850, 20, RangeKind::Spoiler),
            ],
        );
        let r = shown(&t, INITIAL);
        assert!(r.more);
        assert_eq!(
            r.spoilers,
            vec![Span16 {
                start: 790,
                len: 10
            }]
        );
        assert_eq!(r.plain.len(), 800);
    }

    #[test]
    fn invalid_ranges_stay_ignored() {
        // Out of bounds for the whole text: cutting must not make it valid.
        let t = text("a".repeat(1000), vec![range(500, 2000, RangeKind::Bold)]);
        let r = shown(&t, INITIAL);
        assert!(r.more);
        assert_eq!(r.html, "a".repeat(800));
    }

    #[test]
    fn a_part_of_a_long_text_is_never_big_emoji() {
        let t = text(format!("😀{}x", " ".repeat(1000)), vec![]);
        let r = shown(&t, INITIAL);
        assert!(r.more);
        assert!(!r.jumbo);
    }

    #[test]
    fn each_step_adds_three_thousand_and_shows_all_near_the_end() {
        let t = text("a".repeat(4000), vec![]);
        let mut state = ReadMore::default();
        let id = MessageId(7);
        state.begin();
        let first = state.render(id, &t, &TestNames);
        state.end();
        assert_eq!(first.plain.len(), 800);
        assert!(first.more);
        state.raise(id);
        assert_eq!(state.limit(id), 3800);
        state.begin();
        let second = state.render(id, &t, &TestNames);
        state.end();
        assert_eq!(second.plain.len(), 3800);
        assert!(second.more, "200 units are left: more than the buffer");
        state.raise(id);
        state.begin();
        let third = state.render(id, &t, &TestNames);
        state.end();
        assert!(!third.more);
        assert_eq!(third.plain.len(), 4000);
        // 3900 units at the second step: no more than the buffer is left, all is shown.
        let t = text("a".repeat(3900), vec![]);
        let mut state = ReadMore::default();
        state.raise(id);
        assert!(!state.render(id, &t, &TestNames).more);
        // Another message is not affected.
        assert_eq!(state.limit(MessageId(8)), INITIAL);
        state.clear();
        assert_eq!(state.limit(id), INITIAL);
    }

    #[test]
    fn a_cut_text_is_made_again_only_when_something_changed() {
        struct Rename(RefCell<String>);
        impl Names for Rename {
            fn display_name(&self, _user: &UserId) -> String {
                self.0.borrow().clone()
            }
        }
        use kourier_model::text::MENTION_PLACEHOLDER as M;
        let olena = UserId::parse("aci:olena").expect("id");
        let t = text(
            format!("{M}{}", "a".repeat(1000)),
            vec![range(0, M.len_utf8(), RangeKind::Mention(olena))],
        );
        let names = Rename(RefCell::new("Olena".into()));
        let state = ReadMore::default();
        let id = MessageId(1);
        state.begin();
        let a = state.render(id, &t, &names);
        state.end();
        assert!(a.plain.starts_with("@Olena"));
        // Same inputs: the kept result, whatever the cache holds.
        state
            .cache
            .borrow_mut()
            .entries
            .get_mut(&id)
            .expect("kept")
            .rich
            .html = "kept".into();
        state.begin();
        assert_eq!(state.render(id, &t, &names).html, "kept");
        state.end();
        // The mentioned person was renamed: made again.
        *names.0.borrow_mut() = "Oleksandra".into();
        state.begin();
        assert!(
            state
                .render(id, &t, &names)
                .plain
                .starts_with("@Oleksandra")
        );
        state.end();
        // A pass that does not ask for the message forgets it.
        state.begin();
        state.end();
        assert!(state.cache.borrow().entries.is_empty());
    }
}
