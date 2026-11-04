//! WhatsApp's formatting and mentions (W3b, chunk C3). Numbers are the fictitious 555-01xx range.

use kourier_model::text::{MENTION_PLACEHOLDER, validate_range};
use kourier_model::{RangeKind, UserId};

use super::{Marked, Mentioned, mark_up};

fn plain(text: &str) -> Marked {
    mark_up(text, &[])
}

/// (kind, the text the range covers), in the order of the ranges' starts then lengths.
fn shown(marked: &Marked) -> Vec<(String, String)> {
    let mut out: Vec<(u32, u32, String, String)> = marked
        .text
        .ranges
        .iter()
        .map(|r| {
            let slice = &marked.text.body[r.start as usize..(r.start + r.len) as usize];
            let kind = match &r.kind {
                RangeKind::Bold => "bold".to_owned(),
                RangeKind::Italic => "italic".to_owned(),
                RangeKind::Strike => "strike".to_owned(),
                RangeKind::Mono => "mono".to_owned(),
                RangeKind::Mention(user) => format!("mention {}", user.as_str()),
                other => format!("{other:?}"),
            };
            (r.start, r.len, kind, slice.to_owned())
        })
        .collect();
    out.sort_by(|a, b| {
        (a.0, std::cmp::Reverse(a.1), &a.2).cmp(&(b.0, std::cmp::Reverse(b.1), &b.2))
    });
    out.into_iter()
        .map(|(_, _, kind, slice)| (kind, slice))
        .collect()
}

fn check(input: &str, body: &str, ranges: &[(&str, &str)]) {
    let marked = plain(input);
    assert_eq!(marked.text.body, body, "the body of {input:?}");
    let want: Vec<(String, String)> = ranges
        .iter()
        .map(|(k, s)| ((*k).to_owned(), (*s).to_owned()))
        .collect();
    assert_eq!(shown(&marked), want, "the ranges of {input:?}");
    for range in &marked.text.ranges {
        validate_range(&marked.text.body, range).unwrap_or_else(|e| panic!("{input:?}: {e}"));
    }
}

#[test]
fn a_text_with_whatsapps_formatting_becomes_ranges_without_its_markers() {
    // The four markers and the block.
    check("*bold*", "bold", &[("bold", "bold")]);
    check("_it_", "it", &[("italic", "it")]);
    check("~gone~", "gone", &[("strike", "gone")]);
    check("`code`", "code", &[("mono", "code")]);
    check(
        "```block\nof code```",
        "block\nof code",
        &[("mono", "block\nof code")],
    );
    // In a sentence, with punctuation around the pairs.
    check(
        "say *hello* to _all_ ~of~ `you`.",
        "say hello to all of you.",
        &[
            ("bold", "hello"),
            ("italic", "all"),
            ("strike", "of"),
            ("mono", "you"),
        ],
    );
    check("(*bold*)", "(bold)", &[("bold", "bold")]);
    check(
        "*bold*, then *more*!",
        "bold, then more!",
        &[("bold", "bold"), ("bold", "more")],
    );
    // Different markers nest.
    check("*_both_*", "both", &[("bold", "both"), ("italic", "both")]);
    check(
        "*bold _and italic_ text*",
        "bold and italic text",
        &[("bold", "bold and italic text"), ("italic", "and italic")],
    );
    // Inside mono nothing else is read.
    check("`*not bold*`", "*not bold*", &[("mono", "*not bold*")]);
    check("```*x* _y_```", "*x* _y_", &[("mono", "*x* _y_")]);
    // Text that is not a pair stays as typed: mid-word, across a line break, unpaired, a marker next
    // to white space, an empty pair.
    check("a*b*c", "a*b*c", &[]);
    check("snake_case_name", "snake_case_name", &[]);
    check("*two\nlines*", "*two\nlines*", &[]);
    check("*unpaired", "*unpaired", &[]);
    check("5 * 3 * 2", "5 * 3 * 2", &[]);
    check("* not bold *", "* not bold *", &[]);
    check("**", "**", &[]);
    check("``` ```", "``` ```", &[]);
    check("```no end", "```no end", &[]);
    check("_snake_case_", "snake_case", &[("italic", "snake_case")]);
    // List and quote lines stay as typed.
    check("- item *one*", "- item one", &[("bold", "one")]);
    check("> quoted\n1. numbered", "> quoted\n1. numbered", &[]);
    // Astral characters and multi-byte text: the offsets are bytes.
    check("*😀*", "😀", &[("bold", "😀")]);
    check("é *ü* ö", "é ü ö", &[("bold", "ü")]);
    // A block may span lines; a single marker may not.
    check("```a\nb```", "a\nb", &[("mono", "a\nb")]);
    check("`a\nb`", "`a\nb`", &[]);
}

/// A deterministic generator over marker-heavy text (no dependency): a small linear congruential
/// sequence picks characters from an alphabet of markers, white space, letters, digits, `@`, an
/// astral emoji, the placeholder itself and multi-byte letters.
fn generated(count: usize) -> impl Iterator<Item = String> {
    const ALPHABET: &[&str] = &[
        "*", "_", "~", "`", "```", " ", "\n", "a", "b", "1", "@", "@123", ".", "(", ")", "é", "😀",
        "\u{FFFC}", "*_", "_*", "~~", "**",
    ];
    let mut state: u64 = 0x2545_F491_4F6C_DD1D;
    (0..count).map(move |_| {
        let mut next = || {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            usize::try_from(state >> 33).unwrap_or(0)
        };
        let len = next() % 41;
        (0..len)
            .map(|_| ALPHABET[next() % ALPHABET.len()])
            .collect::<String>()
    })
}

#[test]
fn markup_never_panics_and_every_range_is_valid() {
    let people = [
        Mentioned {
            handle: "123".to_owned(),
            user: UserId::from_parts("pn", "15550100101").expect("pn"),
            me: false,
        },
        Mentioned {
            handle: "123".to_owned(),
            user: UserId::from_parts("lid", "99900100101").expect("lid"),
            me: true,
        },
    ];
    let mut seen = 0;
    for input in generated(10_000) {
        for mentioned in [&[][..], &people[..]] {
            let marked = mark_up(&input, mentioned);
            for range in &marked.text.ranges {
                validate_range(&marked.text.body, range)
                    .unwrap_or_else(|e| panic!("{input:?} -> {range:?}: {e}"));
            }
            assert!(
                marked.text.body.chars().count() <= input.chars().count(),
                "{input:?}: the body only gets shorter"
            );
            // A text with none of the markers and no mention comes back byte for byte.
            if mentioned.is_empty() && !input.contains(['*', '_', '~', '`']) {
                assert_eq!(marked.text.body, input);
                assert!(marked.text.ranges.is_empty());
            }
        }
        seen += 1;
    }
    assert_eq!(seen, 10_000);
    // The worst shapes for a scan: long runs of markers and of unpaired openers.
    for input in [
        "*".repeat(100_000),
        "*a ".repeat(30_000),
        "_a_ ".repeat(30_000),
        "`".repeat(100_000),
        "*_~`".repeat(25_000),
        format!("{}x{}", "*_".repeat(500), "_*".repeat(500)),
    ] {
        let marked = plain(&input);
        for range in &marked.text.ranges {
            validate_range(&marked.text.body, range).expect("valid");
        }
    }
}

#[test]
fn a_mention_by_lid_and_one_by_number_are_one_placeholder_each_and_mentions_me_is_set() {
    let lid = UserId::from_parts("lid", "99900100101").expect("lid");
    let pn = UserId::from_parts("pn", "15550100101").expect("pn");
    let mentioned = [
        Mentioned {
            handle: "99900100101".to_owned(),
            user: lid.clone(),
            me: true,
        },
        Mentioned {
            handle: "15550100101".to_owned(),
            user: pn.clone(),
            me: false,
        },
    ];
    let marked = mark_up("hi @99900100101 and @15550100101!", &mentioned);
    let p = MENTION_PLACEHOLDER;
    assert_eq!(marked.text.body, format!("hi {p} and {p}!"));
    assert!(marked.mentions_me, "our own LID is among them");
    assert_eq!(
        shown(&marked),
        [
            ("mention lid:99900100101".to_owned(), p.to_string()),
            ("mention pn:15550100101".to_owned(), p.to_string()),
        ]
    );
    for range in &marked.text.ranges {
        validate_range(&marked.text.body, range).expect("a mention covers one placeholder");
    }

    // Nobody of ours among them.
    let others = [mentioned[1].clone()];
    assert!(!mark_up("@15550100101", &others).mentions_me);

    // A person whose @handle is not in the text is dropped, but a mention of us still counts.
    let marked = mark_up("nobody here", &mentioned);
    assert_eq!(marked.text.body, "nobody here");
    assert!(marked.text.ranges.is_empty());
    assert!(marked.mentions_me);

    // @1234 is not a mention of 123; the same person twice takes the second @handle.
    let short = Mentioned {
        handle: "123".to_owned(),
        user: pn.clone(),
        me: false,
    };
    let marked = mark_up("@1234 @123 @123", &[short.clone(), short.clone()]);
    assert_eq!(marked.text.body, format!("@1234 {p} {p}"));
    assert_eq!(marked.text.ranges.len(), 2);

    // A mention inside bold: both ranges cover the same placeholder.
    let marked = mark_up("*@15550100101* is here", &[mentioned[1].clone()]);
    assert_eq!(marked.text.body, format!("{p} is here"));
    assert_eq!(
        shown(&marked),
        [
            ("bold".to_owned(), p.to_string()),
            ("mention pn:15550100101".to_owned(), p.to_string()),
        ]
    );
}
