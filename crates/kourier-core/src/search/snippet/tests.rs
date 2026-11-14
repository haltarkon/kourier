//! Tests of the snippet of a hit (G1s). The text is synthetic; nothing here is a real chat.

use kourier_model::text::validate_range;
use kourier_model::{
    Attachment, Body, ContactCard, Identifier, IdentifierKind, Location, Placeholder, Poll,
    RangeKind, SystemEvent, Text, TextRange, UserId,
};
use proptest::prelude::*;

use super::super::MIN_PREFIX;
use super::super::normalize::{normalize, query_terms};
use super::*;

fn hit(body: &Body, query: &str) -> Option<Snippet> {
    snippet(body, &query_terms(query))
}

fn slice(text: &str, start: u32, len: u32) -> &str {
    &text[start as usize..(start + len) as usize]
}

/// The marked words of a snippet, in order.
fn marked(s: &Snippet) -> Vec<&str> {
    s.marks
        .iter()
        .map(|m| slice(&s.text, m.start, m.len))
        .collect()
}

/// What the spoiler ranges of a snippet cover.
fn spoiled(s: &Snippet) -> Vec<&str> {
    s.ranges
        .iter()
        .filter(|r| r.kind == RangeKind::Spoiler)
        .map(|r| slice(&s.text, r.start, r.len))
        .collect()
}

/// A text whose `hidden` (its first place) is under a spoiler.
fn with_spoiler(body: &str, hidden: &str) -> Body {
    let start = body.find(hidden).expect("the hidden part is in the body");
    Body::Text(Text {
        body: body.into(),
        ranges: vec![spoiler(start, hidden.len())],
    })
}

fn spoiler(start: usize, len: usize) -> TextRange {
    TextRange {
        start: u32::try_from(start).expect("small"),
        len: u32::try_from(len).expect("small"),
        kind: RangeKind::Spoiler,
    }
}

fn mention(start: usize) -> TextRange {
    TextRange {
        start: u32::try_from(start).expect("small"),
        len: 3,
        kind: RangeKind::Mention(UserId::parse("aci:someone").expect("user id")),
    }
}

fn named(name: &str) -> Attachment {
    Attachment {
        file_name: Some(name.into()),
        ..Attachment::default()
    }
}

/// `n` filler words of four characters with a space each ("w000 w001 ...").
fn filler(from: usize, n: usize) -> String {
    (from..from + n).map(|i| format!("w{i:03} ")).collect()
}

#[test]
fn a_word_of_a_short_text_is_marked_and_the_text_is_whole() {
    let s = hit(&Body::text("Hello wonderful world"), "world").expect("a hit");
    assert_eq!(s.text, "Hello wonderful world");
    assert_eq!(marked(&s), ["world"]);
    assert!(s.ranges.is_empty());
    assert_eq!(s.file, None);
    // Every term is marked where it stands, in the order of the text.
    let s = hit(&Body::text("Hello wonderful world"), "WORLD hello").expect("a hit");
    assert_eq!(marked(&s), ["Hello", "world"]);
}

#[test]
fn a_prefix_term_marks_the_whole_word() {
    let s = hit(&Body::text("Ось наші Файли за тиждень"), "фай").expect("a hit");
    assert_eq!(marked(&s), ["Файли"]);
    // The apostrophe and the folds of the index hold for the snippet too.
    let s = hit(&Body::text("Свіжа м’ята і ґанок"), "мята ганок").expect("a hit");
    assert_eq!(marked(&s), ["м’ята", "ґанок"]);
}

#[test]
fn a_term_shorter_than_the_prefix_length_matches_whole_words_only() {
    if MIN_PREFIX != 2 {
        return;
    }
    assert!(hit(&Body::text("яблуко"), "я").is_none());
    let s = hit(&Body::text("я тут"), "я").expect("a hit");
    assert_eq!(marked(&s), ["я"]);
}

#[test]
fn a_row_that_no_longer_holds_every_term_gives_none() {
    // The index lags a write by a flush; an edit in between leaves a row whose text no
    // longer holds the words: that is no hit.
    let body = Body::text("the new text");
    assert!(hit(&body, "old").is_none());
    assert!(hit(&body, "new old").is_none());
    let s = hit(&body, "new text").expect("a hit");
    assert_eq!(marked(&s), ["new", "text"]);
}

#[test]
fn terms_typed_side_by_side_must_stand_side_by_side() {
    let joined: Vec<bool> = query_terms("日本語").iter().map(|t| t.joined).collect();
    assert_eq!(joined, [false, true, true]);
    let apart: Vec<bool> = query_terms("日 本").iter().map(|t| t.joined).collect();
    assert_eq!(apart, [false, false]);
    let apart: Vec<bool> = query_terms("hello world")
        .iter()
        .map(|t| t.joined)
        .collect();
    assert_eq!(apart, [false, false]);

    let s = hit(&Body::text("日本語を勉強します"), "日本語").expect("a hit");
    assert_eq!(marked(&s), ["日本語"]);
    // The same three ideographs, scattered: FTS5 finds the row (no positions), the snippet
    // drops it.
    let scattered = Body::text("日曜日に本を買った。語");
    assert!(hit(&scattered, "日本語").is_none());
    assert!(
        hit(&scattered, "日 本 語").is_some(),
        "typed apart, found apart"
    );
    // The order counts.
    assert!(hit(&Body::text("本日"), "日本").is_none());
    // Side by side in two pieces of one message is not side by side.
    let two = Body::Media {
        attachments: vec![named("日本")],
        caption: Some(Text::plain("語")),
    };
    assert!(hit(&two, "日本語").is_none());
}

#[test]
fn a_match_under_a_spoiler_stays_a_hit_with_no_mark() {
    let body = with_spoiler("the code is xq7secret, keep it", "xq7secret");
    let s = hit(&body, "xq7secret").expect("still a hit");
    assert!(s.marks.is_empty(), "no mark in a spoiler: {s:?}");
    assert_eq!(spoiled(&s), ["xq7secret"]);
    // A mark that only touches a spoiler is not drawn either; the other term's is.
    let body = with_spoiler("plain words and a secret word", "cret");
    let s = hit(&body, "plain secret").expect("a hit");
    assert_eq!(marked(&s), ["plain"]);
    assert_eq!(spoiled(&s), ["cret"]);
    // Nor one that ends where a spoiler begins, or begins where one ends (privacy LOW-4).
    let body = with_spoiler("plain rareq-secret words", "-secret");
    let s = hit(&body, "plain rareq").expect("a hit");
    assert_eq!(marked(&s), ["plain"], "{s:?}");
    let body = with_spoiler("keep xx;rareq words", "xx;");
    let s = hit(&body, "keep rareq").expect("a hit");
    assert_eq!(marked(&s), ["keep"], "{s:?}");
    // Two blanks between a match and a spoiler that begins at the second become one space of
    // the cut: in the snippet's offsets they touch, so no mark either (the rule holds in the
    // text the marks live in, the proptest's failure of the fix round).
    let body = with_spoiler("plain rareq  secret words", " secret");
    let s = hit(&body, "plain rareq").expect("a hit");
    assert_eq!(marked(&s), ["plain"], "{s:?}");
}

#[test]
fn the_cut_goes_to_a_match_outside_the_spoilers() {
    let source = format!("rarewordx {}then rarewordx again", filler(0, 30));
    let body = with_spoiler(&source, "rarewordx");
    let s = hit(&body, "rarewordx").expect("a hit");
    assert_eq!(marked(&s), ["rarewordx"]);
    assert!(s.text.starts_with(ELLIPSIS), "{s:?}");
    // The spoiler lies wholly before the cut: no range.
    assert!(s.ranges.is_empty(), "{s:?}");
}

#[test]
fn a_spoiler_across_the_start_of_the_cut_is_clipped_to_it() {
    let before = filler(0, 30);
    let source = format!("{before}target after");
    // Hidden: everything before the word but the blank before it (a spoiler that touched the
    // word would take its mark), which the cut starts inside of.
    let body = Body::Text(Text {
        body: source.clone(),
        ranges: vec![spoiler(0, before.trim_end().len())],
    });
    let s = hit(&body, "target").expect("a hit");
    assert_eq!(marked(&s), ["target"]);
    assert_eq!(s.ranges.len(), 1);
    let r = &s.ranges[0];
    assert_eq!(r.start as usize, ELLIPSIS.len_utf8(), "starts at the cut");
    let hidden = spoiled(&s)[0];
    assert!(before.trim_end().ends_with(hidden.trim_end()), "{s:?}");
    assert!(!hidden.contains("target"));
}

#[test]
fn a_mention_keeps_its_range_in_the_cut() {
    let body = Body::Text(Text {
        body: "hi \u{FFFC} look here".into(),
        ranges: vec![mention(3)],
    });
    let s = hit(&body, "look").expect("a hit");
    assert_eq!(s.text, "hi \u{FFFC} look here");
    assert_eq!(s.ranges, vec![mention(3)]);
    // Behind a cut its offsets move with the text.
    let before = filler(0, 25);
    let source = format!("{before}\u{FFFC} look here");
    let body = Body::Text(Text {
        body: source.clone(),
        ranges: vec![mention(before.len())],
    });
    let s = hit(&body, "look").expect("a hit");
    assert!(s.text.starts_with(ELLIPSIS));
    assert_eq!(s.ranges.len(), 1);
    let r = &s.ranges[0];
    assert!(matches!(r.kind, RangeKind::Mention(_)));
    assert_eq!(slice(&s.text, r.start, r.len), "\u{FFFC}");
    // A mention cut off by the window is left out.
    let after = format!("look {}\u{FFFC} end", filler(0, 40));
    let body = Body::Text(Text {
        ranges: vec![mention(after.find('\u{FFFC}').expect("placeholder"))],
        body: after,
    });
    let s = hit(&body, "look").expect("a hit");
    assert!(s.text.ends_with(ELLIPSIS));
    assert!(s.ranges.is_empty(), "{s:?}");
}

#[test]
fn the_cut_ends_on_word_borders_and_keeps_its_bounds() {
    let words: Vec<String> = (0..100).map(|i| format!("word{i:02}")).collect();
    let source = words.join(" ");
    let s = hit(&Body::text(source.clone()), "word50").expect("a hit");
    assert_eq!(marked(&s), ["word50"]);
    assert!(
        s.text.starts_with(ELLIPSIS) && s.text.ends_with(ELLIPSIS),
        "{s:?}"
    );
    let inner = s
        .text
        .trim_start_matches(ELLIPSIS)
        .trim_end_matches(ELLIPSIS);
    assert!(inner.chars().count() <= SNIPPET_CHARS);
    assert!(source.contains(inner));
    assert!(
        inner.split(' ').all(|w| words.iter().any(|x| x == w)),
        "whole words only: {inner}"
    );
    let mark = &s.marks[0];
    let context = &s.text[ELLIPSIS.len_utf8()..mark.start as usize];
    assert!(context.chars().count() <= BEFORE_CHARS);
    assert!(
        context.chars().count() >= BEFORE_CHARS - 8,
        "context is kept"
    );
    // A match near the start keeps the start and fills the rest after it.
    let s = hit(&Body::text(source.clone()), "word01").expect("a hit");
    assert!(s.text.starts_with("word00 word01"));
    assert!(s.text.ends_with(ELLIPSIS));
}

/// The characters of context before the first mark, ellipsis not counted.
fn lead(s: &Snippet) -> usize {
    s.text[..s.marks[0].start as usize]
        .trim_start_matches(ELLIPSIS)
        .chars()
        .count()
}

#[test]
fn the_lead_before_the_match_is_at_most_16_characters_and_starts_a_word() {
    // The match stands at character 60: the cut starts at a word start within 16 before it.
    let source = "alpha bravo charlie delta echo foxtrot golf hotel india juliet kilo lima mike";
    let s = hit(&Body::text(source), "juliet").expect("a hit");
    assert_eq!(marked(&s), ["juliet"]);
    assert!(lead(&s) <= BEFORE_CHARS, "{s:?}");
    assert!(s.text.starts_with(ELLIPSIS), "{s:?}");
    let first = s
        .text
        .trim_start_matches(ELLIPSIS)
        .split(' ')
        .next()
        .unwrap();
    assert!(
        source.split(' ').any(|w| w == first),
        "starts at a word start: {s:?}"
    );
    // A match inside the first 16 characters keeps the start of the text, no ellipsis.
    let s = hit(&Body::text(source), "bravo").expect("a hit");
    assert!(s.text.starts_with("alpha bravo"), "{s:?}");
}

#[test]
fn with_no_word_start_in_reach_the_lead_is_exactly_12_characters() {
    // The index splits "_" inside a word that UAX #29 keeps whole: the match starts mid-segment,
    // so no word border lies in reach.
    let source = format!("{}_needle tail", "x".repeat(40));
    let s = hit(&Body::text(source), "needle").expect("a hit");
    assert!(!s.marks.is_empty(), "{s:?}");
    assert_eq!(lead(&s), BEFORE_CHARS_CUT, "{s:?}");
    assert!(s.text.starts_with(ELLIPSIS), "{s:?}");
}

#[test]
fn right_to_left_text_is_cut_in_logical_order() {
    // Hebrew words, the match is the 12th: the lead counts characters of the logical text.
    let words: Vec<&str> = vec![
        "שלום", "עולם", "גן", "ירק", "חתול", "כלב", "בית", "ספר", "מים", "אור", "ים", "דג", "פרח",
        "עץ",
    ];
    let source = words.join(" ");
    let s = hit(&Body::text(source.clone()), "פרח").expect("a hit");
    assert_eq!(marked(&s), ["פרח"]);
    assert!(lead(&s) <= BEFORE_CHARS, "{s:?}");
    assert!(s.text.starts_with(ELLIPSIS), "{s:?}");
    let first = s
        .text
        .trim_start_matches(ELLIPSIS)
        .split(' ')
        .next()
        .unwrap();
    assert!(words.contains(&first), "starts at a word start: {s:?}");
    // And the same for a mixed line: English, Hebrew, then the match.
    let mixed = format!("{} {source} needle", "plain English words here and there");
    let s = hit(&Body::text(mixed), "needle").expect("a hit");
    assert_eq!(marked(&s), ["needle"]);
    assert!(lead(&s) <= BEFORE_CHARS, "{s:?}");
}

#[test]
fn a_match_longer_than_the_snippet_is_cut_inside() {
    // A word of one letter and 300 accents is one token "a" with a source of 301 characters.
    let long = format!("start {}a{} end", filler(0, 10), "\u{0301}".repeat(300));
    let s = hit(&Body::text(long), "a").expect("a hit");
    assert!(s.text.chars().count() <= SNIPPET_CHARS + 2, "{s:?}");
    assert_eq!(s.marks.len(), 1);
    assert!(s.marks[0].start as usize + s.marks[0].len as usize <= s.text.len());
}

#[test]
fn line_breaks_and_runs_of_white_space_become_one_space() {
    let s = hit(
        &Body::text("first line\n\n  second\tline\r\nthird"),
        "second",
    )
    .expect("a hit");
    assert_eq!(s.text, "first line second line third");
    assert_eq!(marked(&s), ["second"]);
    let body = Body::Text(Text {
        body: "a\n\n\u{FFFC}\u{2028}bee".into(),
        ranges: vec![mention(3)],
    });
    let s = hit(&body, "bee").expect("a hit");
    assert_eq!(s.text, "a \u{FFFC} bee");
    assert_eq!(
        slice(&s.text, s.ranges[0].start, s.ranges[0].len),
        "\u{FFFC}"
    );
    assert_eq!(marked(&s), ["bee"]);
}

#[test]
fn a_file_name_that_matches_names_its_attachment() {
    let body = Body::Media {
        attachments: vec![named("photo.jpg"), named("Report_Q3.pdf")],
        caption: Some(Text::plain("holiday pictures")),
    };
    let s = hit(&body, "report").expect("a hit");
    assert_eq!(s.file, Some(1));
    assert_eq!(s.text, "Report_Q3.pdf");
    assert_eq!(marked(&s), ["Report"]);
    let s = hit(&body, "holiday").expect("a hit");
    assert_eq!(s.file, None);
    assert_eq!(s.text, "holiday pictures");
    // An attachment's caption is a text of its own, not a file name.
    let body = Body::Media {
        attachments: vec![Attachment {
            caption: Some(Text::plain("the second caption")),
            ..named("x.bin")
        }],
        caption: None,
    };
    assert_eq!(hit(&body, "second").expect("a hit").file, None);
}

#[test]
fn the_piece_that_holds_most_of_the_query_shows() {
    let body = Body::Media {
        attachments: vec![named("report budget 2026.pdf")],
        caption: Some(Text::plain("the report is attached")),
    };
    let s = hit(&body, "report budget").expect("a hit");
    assert_eq!(s.file, Some(0));
    assert_eq!(marked(&s), ["report", "budget"]);
    let s = hit(&body, "attached report").expect("a hit");
    assert_eq!(s.file, None);
    assert_eq!(marked(&s), ["report", "attached"]);
    // On a tie the first piece shows.
    let s = hit(&body, "report").expect("a hit");
    assert_eq!(s.file, None);
}

#[test]
fn the_rest_of_a_long_text_is_not_a_file_that_matches() {
    let body = Body::Media {
        attachments: vec![Attachment {
            long_text: true,
            ..named("longwordq.txt")
        }],
        caption: Some(Text::plain("beginning of a long text")),
    };
    assert!(hit(&body, "longwordq").is_none());
    assert!(hit(&body, "beginning").is_some());
}

#[test]
fn a_contact_a_place_and_a_poll_show_their_matching_piece() {
    let card = ContactCard {
        name: Some("Анна Іваненко".into()),
        organization: Some("ТОВ Ромашка".into()),
        identifiers: vec![Identifier {
            kind: IdentifierKind::Phone,
            value: "+380501234567".into(),
        }],
        avatar: None,
    };
    let body = Body::Contact(card);
    let s = hit(&body, "іван").expect("a hit");
    assert_eq!(s.text, "Анна Іваненко");
    assert_eq!(marked(&s), ["Іваненко"]);
    assert_eq!(hit(&body, "ромаш").expect("a hit").text, "ТОВ Ромашка");
    assert!(hit(&body, "380501234567").is_none(), "no number");
    let place = Body::Location(Location {
        lat_e7: 1,
        lon_e7: 2,
        name: Some("Центральний парк".into()),
        address: Some("вул. Шевченка 1".into()),
    });
    assert_eq!(hit(&place, "шевч").expect("a hit").text, "вул. Шевченка 1");
    let poll = Body::Poll(Poll {
        question: "Lunch?".into(),
        options: vec!["Pizza".into(), "Суші".into()],
        multiple: false,
    });
    let s = hit(&poll, "суш").expect("a hit");
    assert_eq!(s.text, "Суші");
    assert_eq!(marked(&s), ["Суші"]);
}

#[test]
fn no_terms_and_the_kinds_never_searched_give_none() {
    let body = Body::text("some words here");
    assert!(snippet(&body, &[]).is_none());
    assert!(hit(&body, "!!! ... ").is_none());
    let sticker = Body::Sticker(Box::new(Attachment {
        caption: Some(Text::plain("stickerword")),
        ..named("stickerword")
    }));
    let notice = Body::System(SystemEvent::MembersAdded {
        by: None,
        users: Vec::new(),
    });
    let unsupported = Body::Unsupported {
        kind_hint: "payment".into(),
        summary: Some("paymentword".into()),
    };
    for body in [
        sticker,
        notice,
        Body::Placeholder(Placeholder::Expired),
        unsupported,
    ] {
        assert!(hit(&body, "stickerword paymentword").is_none());
        assert!(hit(&body, "stickerword").is_none(), "{}", body.kind_name());
        assert!(hit(&body, "paymentword").is_none(), "{}", body.kind_name());
    }
}

#[test]
fn a_spoiler_that_is_not_on_character_borders_hides_more_not_less() {
    // Ranges are sanitised at ingest; one that slipped through is widened, never dropped.
    let body = Body::Text(Text {
        body: "відкрито таємниця тут".into(),
        // Starts in the middle of "т" of "таємниця" and runs past the end of the body.
        ranges: vec![TextRange {
            start: 18,
            len: 1_000,
            kind: RangeKind::Spoiler,
        }],
    });
    let s = hit(&body, "відкрито таємниця").expect("a hit");
    assert_eq!(marked(&s), ["відкрито"]);
    assert_eq!(spoiled(&s), ["таємниця тут"]);
}

#[test]
fn debug_shows_counts_never_a_text() {
    let body = with_spoiler("the secretword is here \u{FFFC}", "here");
    let s = hit(&body, "secretword").expect("a hit");
    let shown = format!("{s:?}");
    assert!(!shown.contains("secretword"), "{shown}");
    assert!(!shown.contains("here"), "{shown}");
    assert!(shown.contains("marks: 1"), "{shown}");
    assert!(shown.contains("spoilers: 1"), "{shown}");
    let term = &query_terms("секрет")[0];
    assert!(!format!("{term:?}").contains("секрет"));
}

/// The rules every snippet keeps, whatever the text: its bounds, valid ranges, marks that
/// are real matches and never touch a spoiler.
fn check_snippet(s: &Snippet, terms: &[QueryTerm]) {
    assert!(s.text.chars().count() <= SNIPPET_CHARS + 2, "{s:?}");
    let mut last_end = None;
    for m in &s.marks {
        let (start, end) = (m.start as usize, (m.start + m.len) as usize);
        assert!(m.len > 0 && end <= s.text.len(), "{s:?}");
        assert!(s.text.is_char_boundary(start) && s.text.is_char_boundary(end));
        assert!(
            last_end.is_none_or(|e| start > e),
            "marks are sorted and apart"
        );
        last_end = Some(end);
        for r in s.ranges.iter().filter(|r| r.kind == RangeKind::Spoiler) {
            let (a, b) = (r.start as usize, (r.start + r.len) as usize);
            assert!(end < a || b < start, "a mark touches a spoiler: {s:?}");
        }
        let words = normalize(&s.text[start..end]);
        assert!(
            words
                .iter()
                .any(|w| terms.iter().any(|t| t.matches(&w.text))),
            "a mark is a match"
        );
    }
    let mut last_start = 0;
    for r in &s.ranges {
        assert_eq!(validate_range(&s.text, r), Ok(()), "{s:?}");
        assert!(r.start >= last_start, "ranges are sorted");
        last_start = r.start;
    }
}

/// Texts of short words from a few scripts, white space of several kinds, mention
/// placeholders, ideographs, and punctuation.
fn wordy() -> impl Strategy<Value = String> {
    let piece = prop_oneof![
        "[a-e]{1,6}",
        "[абвгґ]{1,6}",
        "[日本語を]{1,3}",
        Just(" ".to_owned()),
        Just("\n".to_owned()),
        Just("\u{FFFC}".to_owned()),
        Just(", ".to_owned()),
        Just("м'ята".to_owned()),
    ];
    prop::collection::vec(piece, 0..60).prop_map(|v| v.concat())
}

/// A text with spoilers and mentions on character borders.
fn formatted() -> impl Strategy<Value = (Text, Vec<usize>)> {
    (
        wordy(),
        prop::collection::vec((any::<usize>(), 1usize..30), 0..4),
    )
        .prop_map(|(body, cuts)| {
            let borders: Vec<usize> = body
                .char_indices()
                .map(|(i, _)| i)
                .chain([body.len()])
                .collect();
            let mut ranges: Vec<TextRange> = cuts
                .iter()
                .filter_map(|&(at, len)| {
                    let start = borders[at % borders.len()];
                    let end = borders
                        [(borders.partition_point(|&b| b < start) + len).min(borders.len() - 1)];
                    (start < end).then(|| spoiler(start, end - start))
                })
                .collect();
            ranges.extend(body.match_indices('\u{FFFC}').map(|(i, _)| mention(i)));
            let picks = cuts.iter().map(|&(at, _)| at).collect();
            (Text { body, ranges }, picks)
        })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1000))]

    #[test]
    fn a_word_of_the_text_finds_it_and_the_snippet_keeps_its_rules(
        (text, picks) in formatted()
    ) {
        let tokens = normalize(&text.body);
        let body = Body::Text(text);
        if let Some(&pick) = picks.first().filter(|_| !tokens.is_empty()) {
            let token = &tokens[pick % tokens.len()];
            let terms = query_terms(&token.text);
            let s = snippet(&body, &terms);
            prop_assert!(s.is_some(), "a word of the text finds it");
            check_snippet(&s.expect("checked"), &terms);
        }
        // Any two words of the text, typed apart.
        if tokens.len() >= 2 {
            let query = format!("{} {}", tokens[0].text, tokens[tokens.len() - 1].text);
            let terms = query_terms(&query);
            let s = snippet(&body, &terms);
            prop_assert!(s.is_some());
            check_snippet(&s.expect("checked"), &terms);
        }
    }

    #[test]
    fn any_text_any_query_and_any_ranges_never_panic(
        body in any::<String>(),
        query in any::<String>(),
        raw in prop::collection::vec((any::<u32>(), any::<u32>()), 0..4),
    ) {
        let ranges = raw
            .into_iter()
            .map(|(start, len)| TextRange { start, len, kind: RangeKind::Spoiler })
            .collect();
        let body = Body::Text(Text { body, ranges });
        let terms = query_terms(&query);
        if let Some(s) = snippet(&body, &terms) {
            check_snippet(&s, &terms);
        }
    }
}

#[test]
fn the_fixture_like_line_keeps_its_match_near_the_start() {
    let source =
        "We could train you see train again music tonight garden. Then later we rest a lot.";
    let s = hit(&Body::text(source), "garden").expect("a hit");
    assert_eq!(marked(&s), ["garden"], "{s:?}");
    assert!(lead(&s) <= BEFORE_CHARS, "{s:?} {}", s.text);
}
