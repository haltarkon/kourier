//! `markup` against the synthetic `testdata/receive-entities.jsonl` (every entity type, emoji and CJK
//! offsets, broken offsets, mentions, links) and the texts of `rec-fresh-start.jsonl` (no entities).
//! Assertion messages name what was checked, never the text.

use kourier_model::text::{MENTION_PLACEHOLDER, validate_range};
use kourier_model::{RangeKind, TextRange, UserId};
use serde_json::Value;

use super::{Marked, Me, mark};
use crate::tdapi::{
    FormattedText, Message, Receive, TextEntity, TextEntityType, Update, parse_update,
};

const ENTITIES: &str = include_str!("../../testdata/receive-entities.jsonl");
const FRESH_START: &str = include_str!("../../testdata/rec-fresh-start.jsonl");

/// The synthetic own account of the entity lines: id 100, username `synthetic_1`.
const MY_ID: i64 = 100;

fn usernames() -> Vec<String> {
    vec!["synthetic_1".to_owned()]
}

/// The `formattedText` of the n-th (1-based) message line of `receive-entities.jsonl`.
fn line(n: usize) -> FormattedText {
    let raw = ENTITIES.lines().nth(n - 1).expect("the entity line");
    let Some(Update::Receive(Receive::NewMessage(message))) = parse_update(raw) else {
        panic!("entity line {n} is no updateNewMessage");
    };
    text_of(&message)
}

fn text_of(message: &Message) -> FormattedText {
    serde_json::from_value(message.content.value()["text"].clone()).expect("a formattedText")
}

fn marked(text: &FormattedText) -> Marked {
    let names = usernames();
    mark(
        text,
        Me {
            id: MY_ID,
            usernames: &names,
        },
    )
}

/// The bytes a range covers.
fn covered<'a>(m: &'a Marked, r: &TextRange) -> &'a str {
    &m.text.body[r.start as usize..(r.start + r.len) as usize]
}

/// The covered texts of the ranges of one kind, in order.
fn spans_of(m: &Marked, kind: &RangeKind) -> Vec<String> {
    m.text
        .ranges
        .iter()
        .filter(|r| &r.kind == kind)
        .map(|r| covered(m, r).to_owned())
        .collect()
}

fn tg(id: i64) -> UserId {
    UserId::from_parts("tg", &id.to_string()).expect("a tg id")
}

#[test]
fn utf16_offsets_with_emoji_and_cjk() {
    let text = line(1);
    let m = marked(&text);
    assert_eq!(m.text.body, text.text.as_str(), "the body is the text");
    assert_eq!(m.dropped, 0, "no entity of line 1 is dropped");
    assert_eq!(
        m.text.ranges.len(),
        8,
        "every entity but Underline is a range"
    );
    // After the emoji (2 UTF-16 units, 4 bytes) and the CJK (1 unit, 3 bytes each) the byte offsets
    // differ from the UTF-16 ones: the covered text proves the conversion.
    assert_eq!(spans_of(&m, &RangeKind::Bold), ["bold", "bold 中文"]);
    assert_eq!(spans_of(&m, &RangeKind::Italic), ["中文 italic"]);
    assert_eq!(spans_of(&m, &RangeKind::Strike), ["strike"]);
    assert_eq!(spans_of(&m, &RangeKind::Spoiler), ["hidden"]);
    // Code, Pre and PreCode (its language dropped) are Mono.
    assert_eq!(spans_of(&m, &RangeKind::Mono), ["code", "pre", "block"]);
    assert!(!m.mentions_me);
}

#[test]
fn a_surrogate_split_entity_is_dropped() {
    let text = line(4);
    let m = marked(&text);
    // The text stays whole.
    assert_eq!(m.text.body, text.text.as_str(), "the body is the text");
    // Line 4: Bold starting inside the first emoji, Italic ending inside the second, Strike past the
    // end, Spoiler of no length, Code at a negative offset: dropped. The unknown type is plain (no
    // range, not counted as dropped). The second Bold covers the second emoji whole.
    assert_eq!(m.dropped, 5, "the five broken entities are dropped");
    assert_eq!(m.text.ranges.len(), 1, "only the whole-emoji Bold is left");
    assert_eq!(spans_of(&m, &RangeKind::Bold), ["😀"]);
}

#[test]
fn mention_name_becomes_one_placeholder() {
    let text = line(2);
    let m = marked(&text);
    let p = MENTION_PLACEHOLDER.to_string();
    assert_eq!(
        m.text.body,
        format!("synthetic text 2 hi {p} and 😀 {p}!"),
        "each mentioned span is one placeholder"
    );
    let mentions: Vec<(String, RangeKind)> = m
        .text
        .ranges
        .iter()
        .filter(|r| matches!(r.kind, RangeKind::Mention(_)))
        .map(|r| (covered(&m, r).to_owned(), r.kind.clone()))
        .collect();
    assert_eq!(
        mentions,
        [
            (p.clone(), RangeKind::Mention(tg(202))),
            (p.clone(), RangeKind::Mention(tg(MY_ID)))
        ]
    );
    // A Bold covering a whole mention covers its placeholder; an Italic starting inside a mention is
    // dropped.
    assert_eq!(spans_of(&m, &RangeKind::Bold), [format!("😀 {p}")]);
    assert!(spans_of(&m, &RangeKind::Italic).is_empty());
    assert_eq!(m.dropped, 1, "the Italic inside a mention");
    assert!(m.mentions_me, "a MentionName of our id");
    assert_eq!(m.text.shown_mentions().len(), 2);
}

#[test]
fn two_overlapping_mentions_keep_the_first() {
    let text = FormattedText {
        text: serde_json::from_value(Value::from("synthetic text 6 Bo Peer")).expect("text"),
        entities: vec![
            entity(17, 7, TextEntityType::MentionName { user_id: 202 }),
            entity(20, 4, TextEntityType::MentionName { user_id: 203 }),
            entity(17, 2, TextEntityType::MentionName { user_id: 0 }),
        ],
    };
    let m = marked(&text);
    let p = MENTION_PLACEHOLDER;
    assert_eq!(m.text.body, format!("synthetic text 6 {p}"));
    assert_eq!(m.text.ranges.len(), 1);
    assert_eq!(m.text.ranges[0].kind, RangeKind::Mention(tg(202)));
    assert_eq!(m.dropped, 2, "the overlapping one and the one of no user");
    assert!(!m.mentions_me);
}

fn entity(offset: i32, length: i32, ty: TextEntityType) -> TextEntity {
    TextEntity {
        offset,
        length,
        r#type: ty,
    }
}

/// L-10 (G-4): a hidden link is plain UNLESS the UI shows the target before opening. It does
/// (`TextBlock.qml`: the "Open Link?" dialog shows the url as plain text, only its Open opens it), so
/// an http(s) TextUrl is a `Link` of its url; any other scheme stays plain.
#[test]
fn text_url_is_a_link_since_the_ui_shows_targets() {
    let text = line(3);
    let m = marked(&text);
    assert_eq!(m.text.body, text.text.as_str());
    let links: Vec<(String, RangeKind)> = m
        .text
        .ranges
        .iter()
        .map(|r| (covered(&m, r).to_owned(), r.kind.clone()))
        .collect();
    assert_eq!(
        links,
        [
            (
                "https://example.org/a".to_owned(),
                RangeKind::Link("https://example.org/a".to_owned())
            ),
            (
                "here".to_owned(),
                RangeKind::Link("https://example.org/hidden".to_owned())
            ),
        ],
        "the Url and the https TextUrl are links; ftp and a bare domain stay plain"
    );
    assert_eq!(m.dropped, 0);
}

#[test]
fn unknown_entity_is_plain() {
    let text = line(5);
    let m = marked(&text);
    assert_eq!(m.text.body, text.text.as_str(), "the body is the text");
    assert!(m.text.ranges.is_empty(), "every kind of line 5 is plain");
    assert_eq!(m.dropped, 0);
    // `@synthetic_1` is one of our usernames.
    assert!(m.mentions_me);
    // Another username does not mention us.
    let names = vec!["synthetic_2".to_owned()];
    assert!(
        !mark(
            &text,
            Me {
                id: MY_ID,
                usernames: &names
            }
        )
        .mentions_me
    );
    // Line 4's unknown type parsed as Unknown and is plain too.
    assert!(
        line(4)
            .entities
            .iter()
            .any(|e| e.r#type == TextEntityType::Unknown)
    );
}

#[test]
fn the_recorded_texts_map_unchanged() {
    let mut texts = 0;
    for raw in FRESH_START.lines().filter(|l| !l.trim().is_empty()) {
        let value: Value = serde_json::from_str(raw).expect("a recording line");
        let mut found = Vec::new();
        formatted(&value, &mut found);
        for v in found {
            let text: FormattedText = serde_json::from_value(v.clone()).expect("a formattedText");
            let m = marked(&text);
            assert_eq!(m.text.body, text.text.as_str(), "a recorded text is kept");
            assert!(m.text.ranges.is_empty() && m.dropped == 0);
            texts += 1;
        }
    }
    // Recorded answers used (Q-L11): none (no ScriptedTd); recorded texts read:
    assert!(texts >= 10, "rec-fresh-start holds its texts ({texts})");
    println!("rec-fresh-start texts mapped: {texts}");
}

fn formatted<'a>(v: &'a Value, out: &mut Vec<&'a Value>) {
    match v {
        Value::Array(items) => items.iter().for_each(|i| formatted(i, out)),
        Value::Object(map) => {
            if map.get("@type").and_then(Value::as_str) == Some("formattedText") {
                out.push(v);
            }
            map.values().for_each(|i| formatted(i, out));
        }
        _ => {}
    }
}

/// A small deterministic generator (no new crate: proptest is not a dependency of this crate and the
/// lock must not change): xorshift64*.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

/// Property: for any text and any entities (offsets anywhere, also negative or past the end) every
/// range lies on character boundaries inside the body, a mention covers exactly one placeholder, the
/// body is the text with spans replaced, and `sanitize` drops nothing more.
#[test]
fn ranges_lie_on_char_boundaries() {
    const PIECES: [&str; 8] = ["a", "😀", "中", "é", " ", "\u{FFFC}", "👍🏽", "\n"];
    let kinds = |r: &mut Rng| match r.below(8) {
        0 => TextEntityType::Bold,
        1 => TextEntityType::Italic,
        2 => TextEntityType::Spoiler,
        3 => TextEntityType::Code,
        4 => TextEntityType::Url,
        5 => TextEntityType::MentionName {
            user_id: r.below(4) as i64,
        },
        6 => TextEntityType::Underline,
        _ => TextEntityType::Unknown,
    };
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    for case in 0..3_000 {
        let n = rng.below(24) as usize;
        let body: String = (0..n)
            .map(|_| PIECES[rng.below(PIECES.len() as u64) as usize])
            .collect();
        let len16 = body.encode_utf16().count() as i64;
        let entities = (0..rng.below(8))
            .map(|_| {
                let offset = rng.below(len16 as u64 + 3) as i64 - 1;
                let length = rng.below(len16 as u64 + 2) as i64;
                entity(offset as i32, length as i32, kinds(&mut rng))
            })
            .collect();
        let text = FormattedText {
            text: serde_json::from_value(Value::from(body.clone())).expect("text"),
            entities,
        };
        let m = marked(&text);
        for r in &m.text.ranges {
            assert_eq!(validate_range(&m.text.body, r), Ok(()), "case {case}");
        }
        let mut again = m.text.clone();
        assert_eq!(again.sanitize(), 0, "case {case}");
        let placeholders_in = body.matches(MENTION_PLACEHOLDER).count();
        let mentions = m
            .text
            .ranges
            .iter()
            .filter(|r| matches!(r.kind, RangeKind::Mention(_)))
            .count();
        assert!(
            m.text.body.matches(MENTION_PLACEHOLDER).count() >= mentions,
            "case {case}"
        );
        if mentions == 0 {
            assert_eq!(
                m.text.body, body,
                "case {case}: no mention, the text is kept"
            );
            assert_eq!(
                placeholders_in,
                m.text.body.matches(MENTION_PLACEHOLDER).count()
            );
        }
    }
}

/// `Debug` of a `Marked` prints no text and no link.
#[test]
fn debug_prints_no_text_or_link() {
    for n in 1..=5 {
        let shown = format!("{:?}", marked(&line(n)));
        assert!(
            !shown.contains("synthetic"),
            "line {n}: the text was printed"
        );
        assert!(!shown.contains("example"), "line {n}: a link was printed");
    }
}
