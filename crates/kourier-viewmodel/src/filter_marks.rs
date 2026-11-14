//! Where the RegExp filters' highlight shows (R3): the one function that makes the texts a filter
//! looks at, the marks (UTF-16 ranges and a base colour) of a text or a name, and the marker
//! colour per background. Qt-free; the front end draws the marks as a tinted background.
//!
//! A filter's colour and its Highlight switch come from the COMPILED set (`FilterSet::filters`),
//! never from the stored rows: the indices the matcher gives are into the set. Nothing here logs.

use kourier_core::facade::filters::Field;
use kourier_core::facade::{FilterSet, FilterTexts};

use crate::notify::unmarked_name;
use crate::richtext::{RichText, Span16};

/// One marked range of a text or a name: UTF-16 units of what the front end shows, and the
/// filter's colour as `0xRRGGBB` (the alpha is chosen per background by [`marker_colour`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Mark {
    /// First unit.
    pub start: u32,
    /// Length in units.
    pub len: u32,
    /// The filter's colour, `0xRRGGBB`.
    pub colour: u32,
}

/// THE function that makes the three texts the filters look at (the viewmodel's; the notification
/// path reaches it through the presenter): the message text as shown (markup removed, mentions
/// as `@Name`, the WHOLE text even when "Read more" cuts it), the sender's name as shown without
/// WhatsApp's "~", and the chat's title. `None`: the field is not offered, a filter that uses it
/// does not match.
pub fn filter_texts<'a>(
    text: Option<&'a RichText>,
    sender: Option<&'a str>,
    chat: Option<&'a str>,
) -> FilterTexts<'a> {
    FilterTexts {
        text: text.map(RichText::full_plain),
        sender: sender.map(unmarked_name),
        chat,
    }
}

/// The marks of the Highlight filters among `hits` (indices into the set, list order) in `text`
/// for `field`: the first filter's span wins, a later filter's span is CUT around what is taken
/// (never stacked); a span is clipped to `clip` (the shown part of a cut text), and a span that
/// touches a spoiler is dropped whole (a marker must not show where a hidden word stands).
/// Sorted by start.
pub fn marks_in(
    set: &FilterSet,
    hits: &[usize],
    field: Field,
    text: &str,
    clip: Option<u32>,
    spoilers: &[Span16],
) -> Vec<Mark> {
    let mut taken: Vec<(u32, u32)> = Vec::new();
    let mut out: Vec<Mark> = Vec::new();
    for &i in hits {
        let Some(filter) = set.filters().get(i) else {
            continue;
        };
        if !filter.spec.highlight {
            continue;
        }
        let colour = filter.spec.colour;
        for range in set.spans(i, field, text) {
            let start = u32::try_from(range.start).unwrap_or(u32::MAX);
            let mut end = u32::try_from(range.end).unwrap_or(u32::MAX);
            if let Some(c) = clip {
                end = end.min(c);
            }
            if start >= end {
                continue;
            }
            if spoilers
                .iter()
                .any(|s| start <= s.start.saturating_add(s.len) && end >= s.start)
            {
                continue;
            }
            let mut pieces = vec![(start, end)];
            for &(ts, te) in &taken {
                // Most taken spans touch none of the pieces: no new list then (R6: a pattern
                // that marks every letter made 768,000 lists for 300 titles, 20 ms).
                if !pieces.iter().any(|&(ps, pe)| pe > ts && ps < te) {
                    continue;
                }
                let mut next = Vec::with_capacity(pieces.len() + 1);
                for (ps, pe) in pieces {
                    if pe <= ts || ps >= te {
                        next.push((ps, pe));
                        continue;
                    }
                    if ps < ts {
                        next.push((ps, ts));
                    }
                    if pe > te {
                        next.push((te, pe));
                    }
                }
                pieces = next;
            }
            for (ps, pe) in pieces {
                out.push(Mark {
                    start: ps,
                    len: pe - ps,
                    colour,
                });
                taken.push((ps, pe));
            }
        }
    }
    out.sort_by_key(|m| m.start);
    out
}

/// The marks of a message text: the filters among `hits` that matched the message as a whole,
/// their spans in the WHOLE text, clipped to the shown part (the ellipsis is not marked), none
/// at a spoiler, none on a jumbo emoji.
pub fn text_marks(set: &FilterSet, hits: &[usize], rich: &RichText) -> Vec<Mark> {
    if hits.is_empty() || rich.jumbo {
        return Vec::new();
    }
    let shown = u32::try_from(rich.plain.encode_utf16().count()).unwrap_or(u32::MAX);
    marks_in(
        set,
        hits,
        Field::Text,
        rich.full_plain(),
        Some(shown),
        &rich.spoilers,
    )
}

/// The marks of a sender's name above a bubble: the filters that matched the message as a whole,
/// their sender pattern's spans in the name as shown (the "~" of a chosen name is not part of it
/// and not marked).
pub fn sender_marks(set: &FilterSet, hits: &[usize], name: &str) -> Vec<Mark> {
    let bare = unmarked_name(name);
    let skip =
        u32::try_from(name.encode_utf16().count() - bare.encode_utf16().count()).unwrap_or(0);
    marks_in(set, hits, Field::Sender, bare, None, &[])
        .into_iter()
        .map(|m| Mark {
            start: m.start + skip,
            ..m
        })
        .collect()
}

/// The marks of a chat's title (the list row, the header): every filter whose chat pattern alone
/// matches the title.
pub fn chat_marks(set: &FilterSet, title: &str) -> Vec<Mark> {
    if set.is_empty() || title.is_empty() {
        return Vec::new();
    }
    let hits = set.matches_field(Field::Chat, title);
    marks_in(set, &hits, Field::Chat, title, None, &[])
}

fn channel(c: u32) -> f64 {
    let v = f64::from(c & 0xFF) / 255.0;
    if v <= 0.040_45 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

fn luminance(rgb: u32) -> f64 {
    0.2126 * channel(rgb >> 16) + 0.7152 * channel(rgb >> 8) + 0.0722 * channel(rgb)
}

/// WCAG contrast ratio of two `0xRRGGBB` colours.
pub fn contrast(a: u32, b: u32) -> f64 {
    let (la, lb) = (luminance(a), luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

/// `base` laid over `background` at `alpha` (0..=1), `0xRRGGBB`.
fn over(base: u32, background: u32, alpha: f64) -> u32 {
    let mix = |shift: u32| {
        let b = f64::from((base >> shift) & 0xFF);
        let g = f64::from((background >> shift) & 0xFF);
        (b * alpha + g * (1.0 - alpha)).round().clamp(0.0, 255.0) as u32
    };
    (mix(16) << 16) | (mix(8) << 8) | mix(0)
}

/// The strongest alpha, from 0.45 down to 0.18 in steps of 0.01, at which `text` on `base` laid
/// over `background` still reads at 4.5:1; 0.18 when none does (the text on the plain background
/// is then already below 4.5, the marker stays faint).
pub fn marker_alpha(base: u32, background: u32, text: u32) -> f64 {
    let mut a = 45;
    while a >= 18 {
        let alpha = f64::from(a) / 100.0;
        if contrast(text, over(base, background, alpha)) >= 4.5 {
            return alpha;
        }
        a -= 1;
    }
    0.18
}

/// The marker's colour for a background, as `0xAARRGGBB`.
pub fn marker_colour(base: u32, background: u32, text: u32) -> u32 {
    let alpha = marker_alpha(base, background, text);
    let a = (alpha * 255.0).round() as u32;
    (a << 24) | (base & 0x00FF_FFFF)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filters::SWATCHES;
    use crate::names::NameMap;
    use kourier_core::facade::{Filter, FilterId, FilterSpec};
    use kourier_model::{RangeKind, Redacted, Text, TextRange};

    fn filter(
        i: i64,
        text: Option<&str>,
        sender: Option<&str>,
        chat: Option<&str>,
        colour: u32,
    ) -> Filter {
        let r = |p: Option<&str>| p.map(|p| Redacted::new(p.to_owned()));
        Filter {
            id: FilterId(i),
            spec: FilterSpec {
                name: Redacted::new("f".into()),
                text_pattern: r(text),
                sender_pattern: r(sender),
                chat_pattern: r(chat),
                highlight: true,
                colour,
                ..Default::default()
            },
        }
    }

    fn rich(body: &str, spoiler: Option<(u32, u32)>) -> RichText {
        let ranges = spoiler
            .map(|(start, len)| TextRange {
                start,
                len,
                kind: RangeKind::Spoiler,
            })
            .into_iter()
            .collect();
        crate::richtext::render(
            &Text {
                body: body.into(),
                ranges,
            },
            &NameMap::new(),
        )
    }

    fn run(set: &FilterSet, r: &RichText, sender: Option<&str>) -> Vec<(u32, u32, u32)> {
        let texts = filter_texts(Some(r), sender, Some("Chat"));
        let hits: Vec<usize> = set.matches(&texts).collect();
        text_marks(set, &hits, r)
            .iter()
            .map(|m| (m.start, m.len, m.colour))
            .collect()
    }

    #[test]
    fn a_match_is_marked_in_utf16_units_with_the_filters_colour() {
        let set = FilterSet::build(&[filter(1, Some("яблуко"), None, None, 0x112233)]);
        let r = rich("😀 Яблуко і яблуко", None);
        // The emoji is two units: the words start at 3 and 12.
        assert_eq!(
            run(&set, &r, None),
            vec![(3, 6, 0x112233), (12, 6, 0x112233)]
        );
    }

    #[test]
    fn a_later_filters_span_is_cut_around_the_first_one() {
        let set = FilterSet::build(&[
            filter(1, Some("cde"), None, None, 1),
            filter(2, Some("abcdefg"), None, None, 2),
        ]);
        let r = rich("abcdefg", None);
        assert_eq!(run(&set, &r, None), vec![(0, 2, 2), (2, 3, 1), (5, 2, 2)]);
    }

    #[test]
    fn nothing_in_or_touching_a_spoiler_is_marked() {
        let set = FilterSet::build(&[filter(1, Some("[a-z]+"), None, None, 1)]);
        // "see secret end": the spoiler is the bytes 4..10.
        let r = rich("see secret end", Some((4, 6)));
        let got = run(&set, &r, None);
        assert!(
            got.is_empty() || got.iter().all(|m| m.0 + m.1 < 4 || m.0 > 10),
            "{got:?}"
        );
        let set = FilterSet::build(&[filter(1, Some("see|end"), None, None, 1)]);
        assert_eq!(
            run(&set, &rich("see secret end", Some((4, 6))), None),
            vec![(0, 3, 1), (11, 3, 1)]
        );
    }

    #[test]
    fn a_cut_text_is_matched_whole_and_marked_only_where_it_shows() {
        let set = FilterSet::build(&[filter(1, Some("needle"), None, None, 1)]);
        let body = format!("{} needle", "word ".repeat(200));
        let t = Text {
            body: body.clone(),
            ranges: Vec::new(),
        };
        let r = crate::readmore::render_limited(&t, &NameMap::new(), 100);
        assert!(r.more);
        // The filter matches the whole text, its span lies in the hidden part: no mark.
        let texts = filter_texts(Some(&r), None, None);
        let hits: Vec<usize> = set.matches(&texts).collect();
        assert_eq!(hits, vec![0]);
        assert!(text_marks(&set, &hits, &r).is_empty());
        // A match that straddles the cut is clipped to the shown part.
        let set = FilterSet::build(&[filter(1, Some("word"), None, None, 1)]);
        let hits: Vec<usize> = set.matches(&filter_texts(Some(&r), None, None)).collect();
        let shown = r.plain.encode_utf16().count() as u32;
        let marks = text_marks(&set, &hits, &r);
        assert!(!marks.is_empty());
        assert!(marks.iter().all(|m| m.start + m.len <= shown));
    }

    #[test]
    fn a_filter_with_highlight_off_marks_nothing_in_the_text_the_sender_or_the_title() {
        let mut f = filter(1, Some("hello"), Some("Rika"), Some("Chat"), 5);
        f.spec.highlight = false;
        let set = FilterSet::build(&[f]);
        let r = rich("hello", None);
        let texts = filter_texts(Some(&r), Some("Rika"), Some("Chat"));
        let hits: Vec<usize> = set.matches(&texts).collect();
        assert_eq!(hits, vec![0], "it matches (a Notify filter may use it)");
        assert!(text_marks(&set, &hits, &r).is_empty());
        assert!(sender_marks(&set, &hits, "Rika").is_empty());
        assert!(chat_marks(&set, "Chat").is_empty());
    }

    #[test]
    fn jumbo_emoji_get_no_mark() {
        let set = FilterSet::build(&[filter(1, Some("."), None, None, 1)]);
        let r = rich("😀", None);
        assert!(r.jumbo);
        assert!(run(&set, &r, None).is_empty());
    }

    #[test]
    fn all_filled_fields_must_match_and_a_names_tilde_is_not_marked() {
        let set = FilterSet::build(&[filter(1, Some("hello"), Some("Rika"), None, 7)]);
        let r = rich("hello", None);
        assert!(run(&set, &r, Some("Olga")).is_empty());
        let texts = filter_texts(Some(&r), Some("~Rika Lowe"), Some("Chat"));
        let hits: Vec<usize> = set.matches(&texts).collect();
        assert_eq!(hits, vec![0]);
        let m = sender_marks(&set, &hits, "~Rika Lowe");
        assert_eq!(
            m,
            vec![Mark {
                start: 1,
                len: 4,
                colour: 7
            }]
        );
    }

    #[test]
    fn a_chat_title_is_marked_by_the_chat_pattern_alone() {
        let set = FilterSet::build(&[
            filter(1, Some("zzz"), None, Some("fpv"), 3),
            filter(2, None, None, Some("race"), 4),
        ]);
        let m = chat_marks(&set, "FPV Race club");
        assert_eq!(m.len(), 2);
        assert_eq!((m[0].start, m[0].len, m[0].colour), (0, 3, 3));
        assert_eq!((m[1].start, m[1].len, m[1].colour), (4, 4, 4));
    }

    /// Breeze light, Breeze dark and the two high contrast schemes: (bubble, text).
    const LOOKS: [(u32, u32); 6] = [
        (0xFCFCFC, 0x232627),
        (0xE3E5E7, 0x232627),
        (0x31363B, 0xEFF0F1),
        (0x1B1E20, 0xEFF0F1),
        (0xFFFFFF, 0x000000),
        (0x000000, 0xFFFFFF),
    ];

    #[test]
    fn the_marker_keeps_the_text_readable_on_every_swatch_and_look() {
        for (bubble, text) in LOOKS {
            for base in SWATCHES {
                let alpha = marker_alpha(base, bubble, text);
                assert!((0.18..=0.45).contains(&alpha), "{base:06x} on {bubble:06x}");
                let c = contrast(text, over(base, bubble, alpha));
                assert!(c >= 4.5, "{base:06x} on {bubble:06x}: {c:.2} at {alpha}");
            }
        }
    }

    #[test]
    fn the_marker_is_strong_where_it_can_be_and_the_colour_carries_it() {
        // Black text on white takes the full 0.45 of the yellow.
        assert!((marker_alpha(0xFFD60A, 0xFFFFFF, 0x000000) - 0.45).abs() < 1e-9);
        let c = marker_colour(0xFFD60A, 0xFFFFFF, 0x000000);
        assert_eq!(c & 0x00FF_FFFF, 0xFFD60A);
        assert_eq!(c >> 24, (0.45f64 * 255.0).round() as u32);
    }

    /// R6: what a filter change costs on the Qt thread: every row of a chat (300) and every
    /// title of the list (300) marked again with 50 filters. Run by name in release; prints
    /// counts and microseconds only.
    #[test]
    #[ignore = "a measurement (R6): run by name, in release, under kcap"]
    fn filter_timing_remarking_300_rows_and_300_titles() {
        // (The viewmodel may import the core only through `facade`: a small mix of its own,
        // the same kinds of pattern as the core's `testing::filter_mix`.)
        fn filter_mix_word(k: usize, cyrillic: bool) -> String {
            let (c, v): (&[char], &[char]) = if cyrillic {
                (
                    &[
                        'б', 'д', 'к', 'л', 'м', 'н', 'п', 'р', 'с', 'т', 'в', 'з', 'г', 'х',
                    ],
                    &['а', 'е', 'і', 'о', 'у'],
                )
            } else {
                (
                    &[
                        'b', 'd', 'k', 'l', 'm', 'n', 'p', 'r', 's', 't', 'v', 'z', 'g', 'h',
                    ],
                    &['a', 'e', 'i', 'o', 'u'],
                )
            };
            let mut n = k.wrapping_mul(2_654_435_761) % 70_usize.pow(4);
            let mut out = String::new();
            for _ in 0..4 {
                out.push(c[(n % 70) / 5]);
                out.push(v[n % 5]);
                n /= 70;
            }
            out
        }
        fn filter_mix(n: usize) -> Vec<FilterSpec> {
            (0..n)
                .map(|i| {
                    let text = match i % 6 {
                        0 => filter_mix_word(i, false),
                        1 => (0..5 + (i * 7) % 16)
                            .map(|k| filter_mix_word(i * 31 + k, false))
                            .collect::<Vec<_>>()
                            .join("|"),
                        2 => r"\d{3,}".to_owned(),
                        3 => r"\b\w+@\w+\.\w+\b".to_owned(),
                        4 => format!(
                            r"\b{}\w*",
                            filter_mix_word(i, true).chars().take(3).collect::<String>()
                        ),
                        _ => filter_mix_word(i, true),
                    };
                    let wide = i % 5 == 4;
                    let r = |p: &str| Some(Redacted::new(p.to_owned()));
                    FilterSpec {
                        name: Redacted::new(format!("perf filter {i}")),
                        text_pattern: r(&text),
                        sender_pattern: if wide { r(r"^\w+") } else { None },
                        chat_pattern: if wide { r(r"\w") } else { None },
                        match_case: i % 7 == 0,
                        notify: i % 2 == 0,
                        highlight: true,
                        colour: SWATCHES[i % SWATCHES.len()],
                        ..Default::default()
                    }
                })
                .collect()
        }
        let all: Vec<Filter> = filter_mix(50)
            .into_iter()
            .enumerate()
            .map(|(i, spec)| Filter {
                id: FilterId(i as i64 + 1),
                spec,
            })
            .collect();
        let set = FilterSet::build(&all);
        let rows: Vec<RichText> = (0..300)
            .map(|i| {
                let mut body = String::new();
                for k in 0..(6 + i % 30) {
                    body.push_str(&filter_mix_word(7_000 + i * 40 + k, i % 3 == 0));
                    body.push(' ');
                }
                if i % 25 == 3 {
                    body.push_str(&filter_mix_word(0, false));
                }
                rich(&body, None)
            })
            .collect();
        let titles: Vec<String> = (0..300)
            .map(|i| {
                format!(
                    "{} {}",
                    filter_mix_word(9_000 + i, i % 2 == 0),
                    filter_mix_word(i, false)
                )
            })
            .collect();
        for round in 0..3 {
            let t = std::time::Instant::now();
            let mut marked = 0usize;
            for r in &rows {
                let texts = filter_texts(Some(r), Some("Anna Example"), Some("Family chat"));
                let hits: Vec<usize> = set.matches(&texts).collect();
                marked += text_marks(&set, &hits, r).len();
            }
            let rows_us = t.elapsed().as_secs_f64() * 1e6;
            let t = std::time::Instant::now();
            let mut title_marks = 0usize;
            for title in &titles {
                title_marks += chat_marks(&set, title).len();
            }
            let titles_us = t.elapsed().as_secs_f64() * 1e6;
            println!(
                "filter_timing: round {round}: 50 filters, 300 rows {rows_us:.0} us ({marked} marks), 300 titles {titles_us:.0} us ({title_marks} marks), total {:.0} us",
                rows_us + titles_us
            );
        }
    }
}
