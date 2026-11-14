//! Tests of the token rules (G1a). The text is synthetic; nothing here is a real chat.

use kourier_model::{
    Attachment, Body, ContactCard, Identifier, IdentifierKind, Location, Placeholder, Poll,
    RangeKind, SystemEvent, Text, TextRange, UserId,
};
use proptest::prelude::*;

use super::super::MIN_PREFIX;
use super::*;

fn texts(source: &str) -> Vec<String> {
    normalize(source).into_iter().map(|t| t.text).collect()
}

/// Each row: a source and the tokens it must give.
fn check(rows: &[(&str, &[&str])]) {
    for (source, want) in rows {
        let got = texts(source);
        let got: Vec<&str> = got.iter().map(String::as_str).collect();
        assert_eq!(got, *want, "source {source:?}");
    }
}

/// Each token with the slice of the source it names.
fn sliced(source: &str) -> Vec<(String, &str)> {
    normalize(source)
        .into_iter()
        .map(|t| (t.text, &source[t.start..t.end]))
        .collect()
}

// The Ukrainian apostrophe: five marks, one word. A row for each mark (M10).

#[test]
fn apostrophe_ascii_is_removed_inside_a_word() {
    check(&[
        ("м'ята", &["мята"]),
        ("М'ЯТА", &["мята"]),
        ("мята", &["мята"]),
    ]);
}

#[test]
fn apostrophe_right_single_quote_is_removed_inside_a_word() {
    check(&[("м\u{2019}ята", &["мята"])]);
}

#[test]
fn apostrophe_modifier_letter_is_removed_inside_a_word() {
    check(&[("м\u{02BC}ята", &["мята"])]);
}

#[test]
fn apostrophe_backtick_is_removed_inside_a_word() {
    check(&[("м`ята", &["мята"])]);
}

#[test]
fn apostrophe_acute_accent_is_removed_inside_a_word() {
    // NFKC turns U+00B4 into a space and a combining acute, so it must be mapped first.
    check(&[("м\u{00B4}ята", &["мята"]), ("пор\u{00B4}ята", &["порята"])]);
}

#[test]
fn an_apostrophe_at_the_edge_of_a_word_is_nothing() {
    check(&[
        ("'quoted'", &["quoted"]),
        ("rock 'n' roll", &["rock", "n", "roll"]),
        ("a''b", &["a", "b"]),
        ("`code`", &["code"]),
        ("\u{2019}", &[]),
    ]);
}

#[test]
fn the_apostrophe_marks_that_nfkc_produces_are_mapped_too() {
    // Fullwidth apostrophe -> `'`; fullwidth grave accent and Greek varia -> backtick.
    check(&[
        ("м\u{FF07}ята", &["мята"]),
        ("м\u{FF40}ята", &["мята"]),
        ("м\u{1FEF}ята", &["мята"]),
    ]);
}

#[test]
fn a_number_with_apostrophes_is_one_number() {
    check(&[("1'000'000", &["1000000"]), ("1,000", &["1", "000"])]);
}

#[test]
fn a_raw_word_whose_normal_form_differs() {
    // L2: the attack test searches a raw word that is nowhere in the file in this form.
    check(&[("М'ЯТАXQ", &["мятаxq"])]);
}

// Cyrillic: й and ї are letters, ґ and ё fold, stress marks go (L4).

#[test]
fn i_breve_and_i_diaeresis_stay_letters() {
    check(&[
        ("Йога", &["йога"]),
        ("їжак", &["їжак"]),
        ("и\u{0306}", &["й"]),
        ("і\u{0308}", &["ї"]),
    ]);
    assert_ne!(texts("йому"), texts("иому"), "й is not и");
    assert_ne!(texts("їжак"), texts("ижак"), "ї is not и");
}

#[test]
fn ghe_with_upturn_and_yo_fold() {
    // After the fold a breve may compose with the plain letter: е and a breve is one letter.
    assert_eq!(texts("Ё\u{306}"), texts("е\u{306}"));
    // A second diaeresis, or one behind another mark, never makes a ё again.
    check(&[
        ("Ё\u{308}", &["\u{435}"]),
        ("Ё\u{64E}\u{308}", &["\u{435}\u{64E}"]),
        ("е\u{323}\u{308}\u{308}", &["\u{435}\u{323}"]),
    ]);
    check(&[
        ("ґанок", &["ганок"]),
        ("Ґанок", &["ганок"]),
        ("ёлка", &["елка"]),
        ("Ёж", &["еж"]),
        ("е\u{0308}лка", &["елка"]),
    ]);
}

#[test]
fn cyrillic_stress_marks_go() {
    check(&[
        ("за\u{0301}мок", &["замок"]),
        ("за\u{0300}мо\u{0301}к", &["замок"]),
        ("\u{0453}", &["г"]),
        ("замок", &["замок"]),
    ]);
}

// Latin and Greek.

#[test]
fn latin_and_greek_diacritics_fold() {
    check(&[
        ("café", &["cafe"]),
        ("Crème Brûlée", &["creme", "brulee"]),
        ("cafe\u{0301}", &["cafe"]),
        ("Việt", &["viet"]),
        ("naïve", &["naive"]),
        ("İstanbul", &["istanbul"]),
        ("Ελλάδα", &["ελλαδα"]),
        ("ΐ", &["ι"]),
    ]);
}

#[test]
fn greek_final_sigma_folds() {
    check(&[
        ("λόγος", &["λογοσ"]),
        ("ΛΌΓΟΣ", &["λογοσ"]),
        ("λογοσ", &["λογοσ"]),
    ]);
}

#[test]
fn sharp_s_is_ss() {
    check(&[
        ("Straße", &["strasse"]),
        ("STRASSE", &["strasse"]),
        ("GRO\u{1E9E}", &["gross"]),
    ]);
}

#[test]
fn full_width_forms_ligatures_and_circled_digits_fold_with_nfkc() {
    check(&[
        ("ＡＢＣ１２３", &["abc123"]),
        ("ﬁne", &["fine"]),
        ("ｈｅｌｌｏ\u{3000}ｗｏｒｌｄ", &["hello", "world"]),
        ("①②", &["12"]),
        ("a\u{00A0}b", &["a", "b"]),
    ]);
}

// Hebrew.

const SHALOM_POINTED: &str = "\u{5E9}\u{5C1}\u{5B8}\u{5DC}\u{5D5}\u{5B9}\u{5DD}";
const SHALOM_TOKEN: &str = "\u{5E9}\u{5DC}\u{5D5}\u{5DE}";

#[test]
fn hebrew_points_and_final_forms_fold() {
    check(&[
        (SHALOM_POINTED, &[SHALOM_TOKEN]),
        // Without the points, and the last letter in its medial form: the same token.
        ("\u{5E9}\u{5DC}\u{5D5}\u{5DD}", &[SHALOM_TOKEN]),
        ("\u{5E9}\u{5DC}\u{5D5}\u{5DE}", &[SHALOM_TOKEN]),
        // A cantillation mark (U+0596) goes with the points.
        (
            "\u{5D1}\u{5B0}\u{5BC}\u{5E8}\u{5B5}\u{5D0}\u{5E9}\u{5B4}\u{5C1}\u{596}\u{5D9}\u{5EA}",
            &["\u{5D1}\u{5E8}\u{5D0}\u{5E9}\u{5D9}\u{5EA}"],
        ),
        // The five final forms.
        (
            "\u{5DA}\u{5DD}\u{5DF}\u{5E3}\u{5E5}",
            &["\u{5DB}\u{5DE}\u{5E0}\u{5E4}\u{5E6}"],
        ),
    ]);
}

#[test]
fn hebrew_acronyms_with_gershayim_are_one_word() {
    let acronym = ["\u{5E6}\u{5D4}\u{5DC}"];
    check(&[
        ("\u{5E6}\u{5D4}\u{5F4}\u{5DC}", &acronym),
        ("\u{5E6}\u{5D4}\"\u{5DC}", &acronym),
        (
            "\u{5E6}\u{5D4}\u{201D}\u{5DC}",
            &["\u{5E6}\u{5D4}", "\u{5DC}"],
        ),
    ]);
}

#[test]
fn a_hebrew_geresh_is_dropped_whether_typed_as_geresh_or_apostrophe() {
    let word = ["\u{5D2}\u{5D9}\u{5E8}\u{5E4}"];
    check(&[
        ("\u{5D2}\u{5F3}\u{5D9}\u{5E8}\u{5E3}", &word),
        ("\u{5D2}'\u{5D9}\u{5E8}\u{5E3}", &word),
    ]);
}

#[test]
fn a_hebrew_maqaf_separates_two_words() {
    check(&[(
        "\u{5D1}\u{5D9}\u{5EA}\u{5BE}\u{5E1}\u{5E4}\u{5E8}",
        &["\u{5D1}\u{5D9}\u{5EA}", "\u{5E1}\u{5E4}\u{5E8}"],
    )]);
}

#[test]
fn a_hebrew_word_with_a_particle_in_front_is_one_other_token() {
    // The known limit: no stems. "the house" does not give "house".
    let with_particle = texts("\u{5D4}\u{5D1}\u{5D9}\u{5EA}");
    assert_eq!(with_particle, ["\u{5D4}\u{5D1}\u{5D9}\u{5EA}"]);
    assert_ne!(with_particle, texts("\u{5D1}\u{5D9}\u{5EA}"));
}

// CJK, Hangul, other scripts.

#[test]
fn ideographs_and_hiragana_are_a_word_each_and_katakana_runs_stay_whole() {
    check(&[
        ("日本語", &["日", "本", "語"]),
        ("ひらがな", &["ひ", "ら", "が", "な"]),
        ("カタカナ", &["カタカナ"]),
        ("東京タワー", &["東", "京", "タワー"]),
        ("漢字とカタカナ", &["漢", "字", "と", "カタカナ"]),
    ]);
}

#[test]
fn cjk_tokens_of_one_run_touch_in_the_source() {
    let tokens = normalize("日本語 日 本");
    assert_eq!(
        tokens.iter().map(|t| (t.start, t.end)).collect::<Vec<_>>(),
        [(0, 3), (3, 6), (6, 9), (10, 13), (14, 17)]
    );
}

#[test]
fn hangul_words_are_whole_and_decomposed_jamo_compose() {
    check(&[
        ("한국어 문장", &["한국어", "문장"]),
        ("\u{1112}\u{1161}\u{11AB}", &["\u{D55C}"]),
        ("\u{D55C}", &["\u{D55C}"]),
    ]);
}

#[test]
fn other_scripts_keep_their_letters() {
    check(&[
        ("Привіт, світе!", &["привіт", "світе"]),
        // Hebrew: two words, both final forms folded.
        (
            "\u{5E9}\u{5DC}\u{5D5}\u{5DD} \u{5E2}\u{5D5}\u{5DC}\u{5DD}",
            &[
                "\u{5E9}\u{5DC}\u{5D5}\u{5DE}",
                "\u{5E2}\u{5D5}\u{5DC}\u{5DE}",
            ],
        ),
        // Devanagari: the virama and the vowel signs stay in the word.
        (
            "\u{939}\u{93F}\u{928}\u{94D}\u{926}\u{940}",
            &["\u{939}\u{93F}\u{928}\u{94D}\u{926}\u{940}"],
        ),
        // Arabic and Thai are letters too.
        (
            "\u{627}\u{644}\u{633}\u{644}\u{627}\u{645}",
            &["\u{627}\u{644}\u{633}\u{644}\u{627}\u{645}"],
        ),
    ]);
}

// Punctuation, URLs, numbers.

#[test]
fn urls_split_into_their_words() {
    check(&[
        (
            "https://example.org/path?q=1&r=2",
            &["https", "example", "org", "path", "q", "1", "r", "2"],
        ),
        (
            "Visit www.Example.COM/Foo_bar!",
            &["visit", "www", "example", "com", "foo", "bar"],
        ),
        ("me@example.org", &["me", "example", "org"]),
    ]);
}

#[test]
fn snake_case_splits_and_camel_case_does_not() {
    check(&[
        ("snake_case_name", &["snake", "case", "name"]),
        ("camelCaseName", &["camelcasename"]),
        ("kebab-case", &["kebab", "case"]),
    ]);
}

#[test]
fn numbers_and_phone_numbers() {
    check(&[
        ("+380 50 123 4567", &["380", "50", "123", "4567"]),
        ("3.14", &["3", "14"]),
        ("Q3 2026", &["q3", "2026"]),
    ]);
}

#[test]
fn a_token_over_64_bytes_is_dropped() {
    let at_the_limit = "a".repeat(MAX_TOKEN_BYTES);
    let over = "a".repeat(MAX_TOKEN_BYTES + 1);
    assert_eq!(texts(&at_the_limit), vec![at_the_limit.clone()]);
    assert!(texts(&over).is_empty());
    assert_eq!(texts(&format!("ok {over} fine")), ["ok", "fine"]);
    // Bytes, not characters: 32 Cyrillic letters are 64 bytes, 33 are 66.
    assert_eq!(texts(&"я".repeat(32)).len(), 1);
    assert!(texts(&"я".repeat(33)).is_empty());
    // Base64 of 48 bytes and a hex digest are keys, not words.
    let b64 = "Zm9vYmFyYmF6cXV4Zm9vYmFyYmF6cXV4Zm9vYmFyYmF6cXV4Zm9vYmFyYmF6cXV4Zm9v";
    assert!(b64.len() > MAX_TOKEN_BYTES);
    assert!(texts(b64).is_empty());
    assert!(texts(&"0123456789abcdef".repeat(8)).is_empty());
}

#[test]
fn a_token_is_cut_by_its_folded_size_not_its_raw_size() {
    // 40 letters with two accents each: 120 bytes raw, 40 folded.
    let word = "e\u{0301}\u{0323}".repeat(40);
    assert_eq!(texts(&word), ["e".repeat(40)]);
}

#[test]
fn emoji_and_symbols_are_not_tokens() {
    check(&[
        ("hello \u{1F600} world", &["hello", "world"]),
        (
            "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467} family",
            &["family"],
        ),
        ("\u{1F44D}\u{1F3FD}", &[]),
        ("\u{2605} \u{2713} \u{2192} \u{FFFC}", &[]),
        ("\"*:(){}\u{0}~|^<>[]\\", &[]),
        ("", &[]),
        ("   \n\t ", &[]),
    ]);
}

#[test]
fn invisible_format_characters_neither_split_a_word_nor_stay_in_it() {
    check(&[
        // Persian: with and without the zero-width non-joiner.
        (
            "\u{645}\u{6CC}\u{200C}\u{62E}\u{648}\u{627}\u{647}\u{645}",
            &["\u{645}\u{6CC}\u{62E}\u{648}\u{627}\u{647}\u{645}"],
        ),
        (
            "\u{645}\u{6CC}\u{62E}\u{648}\u{627}\u{647}\u{645}",
            &["\u{645}\u{6CC}\u{62E}\u{648}\u{627}\u{647}\u{645}"],
        ),
        ("abc\u{200E}", &["abc"]),
        ("\u{200F}abc", &["abc"]),
        ("ab\u{00AD}cd", &["abcd"]),
        ("ab\u{2060}cd", &["abcd"]),
        ("\u{FEFF}abc", &["abc"]),
        // The zero-width space IS a separator.
        ("foo\u{200B}bar", &["foo", "bar"]),
        // A mark that a removed character stood in front of still belongs to its letter.
        ("e\u{200E}\u{0301}", &["e"]),
    ]);
}

#[test]
fn text_meant_for_fts5_syntax_is_only_words() {
    check(&[
        ("NEAR(a b)", &["near", "a", "b"]),
        ("a AND b OR c", &["a", "and", "b", "or", "c"]),
        ("words:foo*", &["words", "foo"]),
        ("\"quoted phrase\"", &["quoted", "phrase"]),
        ("{x} (y) - z", &["x", "y", "z"]),
    ]);
}

#[test]
fn punctuation_that_joins_two_letters_splits_them() {
    // The middle dot and the fraction slash join letters and digits in UAX #29; a token with
    // one at its edge would fall apart when read again, so they split like any punctuation.
    check(&[
        ("col\u{B7}legi", &["col", "legi"]),
        ("\u{5F3}\u{B7}\u{435}", &["\u{435}"]),
        ("1\u{2044}2", &["1", "2"]),
        (
            "\u{30FB}\u{30A2}\u{30FB}\u{30A4}",
            &["\u{30A2}", "\u{30A4}"],
        ),
        ("a\u{203F}b", &["a", "b"]),
    ]);
}

#[test]
fn a_lone_combining_mark_is_not_a_token() {
    check(&[
        ("\u{0301}", &[]),
        ("\u{0301}abc", &["abc"]),
        ("a_\u{0301}b", &["a", "b"]),
        // A mark of any script after a separator belongs to the separator (UAX #29 glues it
        // there), never to the next token: Gujarati sign aa, Arabic fatha, a virama.
        ("a.\u{0ABE}b", &["a", "b"]),
        ("\u{491}.\u{64E}a", &["\u{433}", "a"]),
        ("a_\u{94D}b", &["a", "b"]),
        // A removed character (geresh, bidi mark) in front of a mark does not leave it a token.
        ("\u{5F3}\u{200E}\u{64E}\u{491}", &["\u{433}"]),
        // A mark goes with the character that carried it: the fatha of a removed apostrophe is
        // not the end of the word in front of it.
        ("\u{435}\u{FF07}\u{64E}\u{5F3}\u{5F3}", &["\u{435}"]),
        ("a'\u{0ABE}b", &["ab"]),
        // But after a letter a mark stays with it.
        ("a\u{200E}\u{0ABE}b", &["a\u{0ABE}b"]),
    ]);
}

// Source ranges (M12).

#[test]
fn a_token_names_its_source() {
    assert_eq!(
        sliced("Hello, wörld"),
        [("hello".into(), "Hello"), ("world".into(), "wörld")]
    );
    assert_eq!(
        sliced("м'ята і ґанок"),
        [
            ("мята".into(), "м'ята"),
            ("і".into(), "і"),
            ("ганок".into(), "ґанок")
        ]
    );
    // A mark stays with its letter; an apostrophe or a format character at the edge does not
    // belong to the word.
    assert_eq!(
        sliced("cafe\u{0301} ok"),
        [("cafe".into(), "cafe\u{0301}"), ("ok".into(), "ok")]
    );
    assert_eq!(sliced("'quoted'"), [("quoted".into(), "quoted")]);
    assert_eq!(sliced("\u{200E}abc\u{200E}"), [("abc".into(), "abc")]);
    assert_eq!(
        sliced("a.b/c"),
        [("a".into(), "a"), ("b".into(), "b"), ("c".into(), "c")]
    );
    assert_eq!(
        sliced("x \u{5E9}\u{5C1}\u{5B8}\u{5DC} y"),
        [
            ("x".into(), "x"),
            ("\u{5E9}\u{5DC}".into(), "\u{5E9}\u{5C1}\u{5B8}\u{5DC}"),
            ("y".into(), "y")
        ]
    );
}

#[test]
fn one_cluster_that_nfkc_expands_gives_its_tokens_one_range() {
    // The limit of the range property: U+2105 is "c/o" for NFKC.
    let tokens = normalize("\u{2105}");
    assert_eq!(
        tokens.iter().map(|t| t.text.as_str()).collect::<Vec<_>>(),
        ["c", "o"]
    );
    assert!(tokens.iter().all(|t| (t.start, t.end) == (0, 3)));
}

#[test]
fn debug_shows_counts_never_a_text() {
    let token = &normalize("секрет")[0];
    let shown = format!("{token:?}");
    assert!(!shown.contains("секрет"), "{shown}");
    assert!(shown.contains("bytes: 12"), "{shown}");
    let term = &query_terms("секрет")[0];
    let shown = format!("{term:?}");
    assert!(!shown.contains("секрет"), "{shown}");
    assert!(shown.contains("chars: 6"), "{shown}");
}

// words(&Body): what goes into the index.

fn word_list(body: &Body) -> Vec<String> {
    words(body)
        .map(|w| w.split(' ').map(str::to_owned).collect())
        .unwrap_or_default()
}

fn text_with_a_mention() -> Body {
    let user = UserId::parse("aci:me").expect("user id");
    Body::Text(Text {
        body: "hi \u{FFFC} there".into(),
        ranges: vec![TextRange {
            start: 3,
            len: 3,
            kind: RangeKind::Mention(user),
        }],
    })
}

#[test]
fn a_text_gives_its_tokens() {
    assert_eq!(
        words(&Body::text("Hello, Wörld! See https://example.org/x")).as_deref(),
        Some("hello world see https example org x")
    );
    // A mention's name is not in the body (U+FFFC), so it is not indexed.
    assert_eq!(words(&text_with_a_mention()).as_deref(), Some("hi there"));
    assert_eq!(words(&Body::text("\u{1F600}\u{1F600}")), None);
    assert_eq!(words(&Body::text("")), None);
}

#[test]
fn a_media_message_gives_caption_attachment_captions_and_file_names() {
    let named = |name: &str| Attachment {
        file_name: Some(name.into()),
        ..Attachment::default()
    };
    let body = Body::Media {
        attachments: vec![
            named("Report_Q3.pdf"),
            Attachment {
                caption: Some(Text::plain("Second caption")),
                ..Attachment::default()
            },
        ],
        caption: Some(Text::plain("First caption")),
    };
    assert_eq!(
        words(&body).as_deref(),
        Some("first caption report q3 pdf second caption")
    );
    let nameless = Body::Media {
        attachments: vec![Attachment::default()],
        caption: None,
    };
    assert_eq!(words(&nameless), None);
}

#[test]
fn the_name_of_the_rest_of_a_long_text_is_not_indexed() {
    // L3: the attachment that only carries the rest of a long text is not a file of his.
    let body = Body::Media {
        attachments: vec![
            Attachment {
                long_text: true,
                file_name: Some("signal-long-message-uniqueword.txt".into()),
                ..Attachment::default()
            },
            Attachment {
                file_name: Some("photo.jpg".into()),
                ..Attachment::default()
            },
        ],
        caption: Some(Text::plain("Beginning of a long text")),
    };
    let words = word_list(&body);
    assert!(!words.iter().any(|w| w == "uniqueword" || w == "signal"));
    assert!(words.iter().any(|w| w == "photo"));
    assert!(words.iter().any(|w| w == "beginning"));
}

#[test]
fn a_contact_card_gives_its_name_and_organization_and_no_number() {
    let card = ContactCard {
        name: Some("Анна Іваненко".into()),
        organization: Some("ТОВ Ромашка".into()),
        identifiers: vec![
            Identifier {
                kind: IdentifierKind::Phone,
                value: "+380501234567".into(),
            },
            Identifier {
                kind: IdentifierKind::Email,
                value: "anna@uniquemail.example".into(),
            },
            Identifier {
                kind: IdentifierKind::Username,
                value: "annaiv".into(),
            },
        ],
        avatar: None,
    };
    assert_eq!(
        words(&Body::Contact(card)).as_deref(),
        Some("анна іваненко тов ромашка")
    );
    assert_eq!(words(&Body::Contact(ContactCard::default())), None);
}

#[test]
fn a_location_gives_its_name_and_address_and_no_coordinates() {
    let place = Location {
        lat_e7: 481_234_567,
        lon_e7: 241_234_567,
        name: Some("Центральний парк".into()),
        address: Some("вул. Шевченка 1".into()),
    };
    assert_eq!(
        words(&Body::Location(place)).as_deref(),
        Some("центральний парк вул шевченка 1")
    );
    let bare = Location {
        lat_e7: 1,
        lon_e7: 2,
        ..Location::default()
    };
    assert_eq!(words(&Body::Location(bare)), None);
}

#[test]
fn a_poll_gives_its_question_and_options_each_apart() {
    let poll = Poll {
        question: "Lunch?".into(),
        options: vec!["Pizza".into(), "Суші".into()],
        multiple: false,
    };
    assert_eq!(
        words(&Body::Poll(poll)).as_deref(),
        Some("lunch pizza суші")
    );
    // Pieces never run together into one token.
    let tight = Poll {
        question: "a".into(),
        options: vec!["b".into()],
        multiple: false,
    };
    assert_eq!(words(&Body::Poll(tight)).as_deref(), Some("a b"));
}

#[test]
fn the_other_kinds_are_never_indexed() {
    let words_in = |text: &str| {
        Some(Attachment {
            caption: Some(Text::plain(text)),
            file_name: Some(text.into()),
            ..Attachment::default()
        })
    };
    let sticker = Body::Sticker(Box::new(words_in("stickerword").expect("attachment")));
    let notice = Body::System(SystemEvent::MembersAdded {
        by: None,
        users: Vec::new(),
    });
    let placeholder = Body::Placeholder(Placeholder::Expired);
    let unsupported = Body::Unsupported {
        kind_hint: "payment".into(),
        summary: Some("paymentword".into()),
    };
    for body in [sticker, notice, placeholder, unsupported] {
        assert_eq!(words(&body), None, "{}", body.kind_name());
        assert!(searched_texts(&body).is_empty(), "{}", body.kind_name());
    }
}

// Query terms.

#[test]
fn a_query_is_cut_like_the_index_and_marks_its_prefixes() {
    let terms = query_terms("Привіт, Світ!");
    assert_eq!(
        terms.iter().map(|t| t.text.as_str()).collect::<Vec<_>>(),
        ["привіт", "світ"]
    );
    assert!(terms.iter().all(|t| t.prefix));
    assert_eq!(query_terms("м'ята")[0].text, "мята");
}

#[test]
fn a_term_is_a_prefix_from_min_prefix_characters() {
    // The tests follow the constant, so his answer on short letters changes one number.
    let longest_exact = "x".repeat(MIN_PREFIX - 1);
    let shortest_prefix = "x".repeat(MIN_PREFIX);
    let exact = query_terms(&longest_exact);
    let prefix = query_terms(&shortest_prefix);
    assert_eq!(exact.len(), usize::from(MIN_PREFIX > 1));
    assert!(exact.iter().all(|t| !t.prefix));
    assert_eq!((prefix.len(), prefix[0].prefix), (1, true));
    // Characters, not bytes: one Cyrillic letter is two bytes and still one letter.
    if MIN_PREFIX == 2 {
        assert!(!query_terms("я")[0].prefix);
        assert!(query_terms("яя")[0].prefix);
        assert!(!query_terms("日")[0].prefix);
    }
}

#[test]
fn a_query_keeps_32_terms_and_512_bytes() {
    assert_eq!(query_terms(&"ab ".repeat(100)).len(), MAX_QUERY_TERMS);
    let late = format!("{}needle", " ".repeat(MAX_QUERY_BYTES));
    assert!(
        query_terms(&late).is_empty(),
        "the word after byte 512 is cut"
    );
    let early = format!("needle{}", " ".repeat(MAX_QUERY_BYTES));
    assert_eq!(query_terms(&early).len(), 1);
    // The cut never lands inside a character.
    assert_eq!(query_terms(&"日".repeat(300)).len(), MAX_QUERY_TERMS);
}

#[test]
fn a_query_of_punctuation_has_no_terms() {
    for query in [
        "",
        " ",
        "\"",
        "*",
        ":",
        "(",
        ")",
        "{",
        "\u{0}",
        "\"*:(){}\u{0}",
        "\u{FFFC}",
    ] {
        assert!(query_terms(query).is_empty(), "{query:?}");
    }
    assert!(query_terms(&"(".repeat(100_000)).is_empty());
}

// Properties.

fn nasty(with_expanders: bool) -> impl Strategy<Value = String> {
    let mut pool: Vec<char> = vec![
        'a',
        'B',
        'z',
        '7',
        ' ',
        '.',
        '_',
        '-',
        '/',
        ':',
        '"',
        '*',
        '(',
        '\n',
        '\u{0}',
        '\'',
        '`',
        '\u{2019}',
        '\u{2BC}',
        '\u{B4}',
        'е',
        'Ё',
        'й',
        'и',
        '\u{306}',
        'ї',
        'і',
        '\u{308}',
        'ґ',
        'м',
        'я',
        'т',
        'а',
        '\u{301}',
        '\u{300}',
        '\u{323}',
        'é',
        'e',
        'ß',
        'ς',
        'Σ',
        'İ',
        'ł',
        'ǆ',
        '\u{5E9}',
        '\u{5C1}',
        '\u{5B8}',
        '\u{5DD}',
        '\u{5DC}',
        '\u{5F3}',
        '\u{5F4}',
        '\u{5BE}',
        '\u{596}',
        '日',
        '本',
        'ひ',
        'カ',
        'ー',
        '한',
        '\u{1112}',
        '\u{1161}',
        '\u{11AB}',
        '\u{200B}',
        '\u{200C}',
        '\u{200D}',
        '\u{200E}',
        '\u{AD}',
        '\u{FEFF}',
        '\u{FE0F}',
        '\u{1F600}',
        '\u{FFFC}',
        'Ａ',
        '１',
        'ﬁ',
        '①',
        '\u{3000}',
        '\u{A0}',
        '\u{627}',
        '\u{64E}',
        'क',
        '\u{94D}',
        'ก',
        '·',
        '\u{1FFD}',
        '\u{FF07}',
        '\u{FF40}',
    ];
    if with_expanders {
        // NFKC turns these into several tokens at once: "a/c", "c/o", four ideographs, "1⁄2".
        pool.extend([
            '\u{2100}', '\u{2105}', '\u{337F}', '\u{BD}', '\u{FDFA}', '\u{2474}',
        ]);
    }
    prop::collection::vec(prop::sample::select(pool), 0..48)
        .prop_map(|chars: Vec<char>| chars.into_iter().collect::<String>())
}

/// What every token of every text must satisfy.
fn check_tokens(source: &str) {
    let tokens = normalize(source);
    let mut last = (0, 0);
    for t in &tokens {
        assert!(!t.text.is_empty() && t.text.len() <= MAX_TOKEN_BYTES);
        assert!(
            t.text
                .chars()
                .all(|c| !c.is_ascii() || c.is_ascii_alphanumeric()),
            "an ASCII character that is not a letter or a digit"
        );
        assert!(!t.text.chars().any(|c| c.is_ascii_uppercase()));
        assert!(t.text.chars().any(char::is_alphanumeric));
        assert!(t.start <= t.end && t.end <= source.len());
        assert!(source.is_char_boundary(t.start) && source.is_char_boundary(t.end));
        assert!(t.start >= last.0 && t.end >= last.1, "tokens go forward");
        last = (t.start, t.end);
        // A word is found by its own query: the one token, a prefix or not.
        let own = query_terms(&t.text);
        assert_eq!(own.len(), 1);
        assert_eq!(own[0].text, t.text);
        // The range's own normalisation still holds the token.
        assert!(
            normalize(&source[t.start..t.end])
                .iter()
                .any(|a| a.text == t.text),
            "the range of a token gives the token again"
        );
    }
    // Idempotent: the tokens, normalised again, are the same tokens.
    let words: Vec<&str> = tokens.iter().map(|t| t.text.as_str()).collect();
    let again = texts(&words.join(" "));
    assert_eq!(again, words, "normalising the tokens again changed them");
}

/// The exact form of M12, for text with no NFKC expansion that breaks into tokens.
fn check_ranges_exactly(source: &str) {
    for t in normalize(source) {
        let again = normalize(&source[t.start..t.end]);
        assert!(
            again.len() == 1 && again[0].text == t.text,
            "a range gives its token and no other"
        );
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2000))]

    #[test]
    fn any_text_gives_clean_tokens(source in any::<String>()) {
        check_tokens(&source);
    }

    #[test]
    fn a_text_of_hard_characters_gives_clean_tokens(source in nasty(true)) {
        check_tokens(&source);
    }

    #[test]
    fn a_range_gives_its_token_and_no_other(source in nasty(false)) {
        check_ranges_exactly(&source);
    }

    #[test]
    fn arbitrary_bytes_of_a_query_never_panic(source in any::<String>()) {
        let terms = query_terms(&source);
        prop_assert!(terms.len() <= MAX_QUERY_TERMS);
    }
}
