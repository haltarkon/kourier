//! Property tests of the contract's pure helpers: `Utf16Map`, range validation, id grammars,
//! `Patch` semantics and `Redacted`.

use kourier_model::text::{MENTION_PLACEHOLDER, validate_range};
use kourier_model::{
    ConvKey, NetMsgId, Patch, RangeKind, Redacted, Text, TextRange, UserId, Utf16Map,
};
use proptest::prelude::*;
use proptest::sample::Index;
use serde::{Deserialize, Serialize};

/// Pieces that stress UTF-8/UTF-16 offsets: ASCII, 2- and 3-byte BMP characters, ZWJ,
/// variation selectors, the mention placeholder, astral emoji, skin tones, flags and full ZWJ
/// sequences, plus arbitrary scalar values.
fn piece() -> impl Strategy<Value = String> {
    let bmp = ["é", "ß", "€", "中", "\u{200D}", "\u{FE0F}", "\u{FFFC}"];
    let astral = [
        "😀",
        "𝄞",
        "👍🏽",
        "🇩🇪",
        "👨\u{200D}👩\u{200D}👧\u{200D}👦",
        "🏳\u{FE0F}\u{200D}🌈",
        "👩🏽\u{200D}💻",
    ];
    prop_oneof![
        3 => "[a-z ]{1,3}",
        2 => prop::sample::select(bmp.to_vec()).prop_map(String::from),
        3 => prop::sample::select(astral.to_vec()).prop_map(String::from),
        1 => any::<char>().prop_map(String::from),
    ]
}

fn text_body() -> impl Strategy<Value = String> {
    prop::collection::vec(piece(), 0..24).prop_map(|v| v.concat())
}

/// Every UTF-8 character boundary of `s`, including `s.len()`.
fn boundaries(s: &str) -> Vec<usize> {
    s.char_indices()
        .map(|(i, _)| i)
        .chain(std::iter::once(s.len()))
        .collect()
}

fn as_u32(n: usize) -> u32 {
    u32::try_from(n).expect("test strings are short")
}

proptest! {
    #[test]
    fn utf16_map_agrees_with_std(s in text_body()) {
        let m = Utf16Map::new(&s);
        let units: Vec<u16> = s.encode_utf16().collect();
        prop_assert_eq!(m.utf8_len(), s.len());
        prop_assert_eq!(m.utf16_len(), units.len());
        for i in 0..=s.len() + 2 {
            let want = s
                .get(..i)
                .map(|prefix| as_u32(prefix.encode_utf16().count()));
            prop_assert_eq!(m.to_utf16(as_u32(i)), want, "utf8 offset {}", i);
        }
        for j in 0..=units.len() + 2 {
            // A prefix ending between the halves of a surrogate pair does not decode.
            let want = units
                .get(..j)
                .and_then(|prefix| String::from_utf16(prefix).ok())
                .map(|prefix| as_u32(prefix.len()));
            prop_assert_eq!(m.to_utf8(as_u32(j)), want, "utf16 offset {}", j);
        }
    }

    #[test]
    fn utf16_ranges_roundtrip_and_cover_the_same_text(
        s in text_body(),
        a in any::<Index>(),
        b in any::<Index>(),
    ) {
        let bounds = boundaries(&s);
        let (x, y) = (a.get(&bounds), b.get(&bounds));
        let (start, end) = (*x.min(y), *x.max(y));
        let m = Utf16Map::new(&s);
        let (start16, len16) = m
            .range_to_utf16(as_u32(start), as_u32(end - start))
            .expect("boundaries map");
        let units: Vec<u16> = s.encode_utf16().collect();
        let slice16 = &units[start16 as usize..(start16 + len16) as usize];
        prop_assert_eq!(String::from_utf16(slice16).ok(), Some(s[start..end].to_owned()));
        prop_assert_eq!(
            m.range_to_utf8(start16, len16),
            Some((as_u32(start), as_u32(end - start)))
        );
    }
}

fn range_kind() -> impl Strategy<Value = RangeKind> {
    prop_oneof![
        Just(RangeKind::Bold),
        Just(RangeKind::Italic),
        Just(RangeKind::Strike),
        Just(RangeKind::Spoiler),
        Just(RangeKind::Mono),
        Just(RangeKind::MentionAll),
        Just(RangeKind::Mention(
            UserId::parse("aci:00000000-0000-4000-8000-000000000001").expect("valid")
        )),
        Just(RangeKind::Link("https://example.org/".into())),
    ]
}

fn offset() -> impl Strategy<Value = u32> {
    prop_oneof![8 => 0u32..96, 1 => Just(u32::MAX), 1 => Just(u32::MAX - 1)]
}

/// Independent oracle for range validity.
fn oracle_valid(body: &str, r: &TextRange) -> bool {
    let start = r.start as usize;
    let end = start + r.len as usize;
    if r.len == 0 {
        return false;
    }
    match body.get(start..end) {
        None => false,
        Some(slice) => match r.kind {
            RangeKind::Mention(_) | RangeKind::MentionAll => {
                let mut chars = slice.chars();
                chars.next() == Some(MENTION_PLACEHOLDER) && chars.next().is_none()
            }
            _ => true,
        },
    }
}

proptest! {
    #[test]
    fn sanitize_keeps_exactly_the_valid_ranges(
        body in text_body(),
        raw in prop::collection::vec((offset(), offset(), range_kind()), 0..12),
    ) {
        let ranges: Vec<TextRange> = raw
            .into_iter()
            .map(|(start, len, kind)| TextRange { start, len, kind })
            .collect();
        let valid: Vec<TextRange> =
            ranges.iter().filter(|r| oracle_valid(&body, r)).cloned().collect();
        for r in &ranges {
            prop_assert_eq!(validate_range(&body, r).is_ok(), oracle_valid(&body, r));
        }
        let mut t = Text { body: body.clone(), ranges: ranges.clone() };
        prop_assert_eq!(t.sanitize(), ranges.len() - valid.len());
        prop_assert_eq!(&t.ranges, &valid);
        prop_assert_eq!(t.sanitize(), 0, "idempotent");
        // Every kept range converts to UTF-16 (the Signal and Qt edges) and back.
        let m = t.utf16_map();
        for r in &t.ranges {
            let (s16, l16) = m.range_to_utf16(r.start, r.len).expect("valid range maps");
            prop_assert_eq!(m.range_to_utf8(s16, l16), Some((r.start, r.len)));
        }
    }
}

fn scheme() -> impl Strategy<Value = String> {
    "[a-z][a-z0-9-]{0,15}"
}

/// Id values: no control or whitespace characters; `:`, `@` and `%` are allowed.
fn id_value() -> impl Strategy<Value = String> {
    prop_oneof!["[^\\s\\p{Cc}]{1,40}", "[a-z0-9:@%+=_-]{1,20}"]
}

fn user_id() -> impl Strategy<Value = UserId> {
    (scheme(), id_value()).prop_map(|(s, v)| UserId::from_parts(&s, &v).expect("valid parts"))
}

fn local_part() -> impl Strategy<Value = String> {
    prop_oneof!["[^\\p{Cc}]{1,40}", "[a-z0-9@% ]{1,12}"]
}

fn scoped_id() -> impl Strategy<Value = String> {
    (scheme(), id_value()).prop_map(|(s, v)| format!("{s}:{v}"))
}

fn conv_key() -> impl Strategy<Value = ConvKey> {
    prop_oneof![
        user_id().prop_map(ConvKey::Direct),
        scoped_id().prop_map(|s| ConvKey::Group(s.into())),
        scoped_id().prop_map(|s| ConvKey::Broadcast(s.into())),
        scoped_id().prop_map(|s| ConvKey::Channel(s.into())),
    ]
}

fn json_roundtrip<T>(v: &T) -> Result<T, TestCaseError>
where
    T: Serialize + for<'de> Deserialize<'de>,
{
    let json = serde_json::to_string(v).map_err(|e| TestCaseError::fail(e.to_string()))?;
    serde_json::from_str(&json).map_err(|e| TestCaseError::fail(e.to_string()))
}

proptest! {
    #[test]
    fn user_id_grammar_roundtrips(scheme in scheme(), value in id_value()) {
        let s = format!("{scheme}:{value}");
        let u = UserId::parse(&s).expect("valid");
        prop_assert_eq!(u.as_str(), s.as_str());
        prop_assert_eq!(u.scheme(), scheme.as_str());
        prop_assert_eq!(u.value(), value.as_str());
        prop_assert_eq!(s.parse::<UserId>().ok(), Some(u.clone()));
        prop_assert_eq!(json_roundtrip(&u)?, u);
    }

    #[test]
    fn conv_key_grammar_roundtrips(k in conv_key()) {
        prop_assert_eq!(k.validate(), Ok(()));
        let s = k.encode();
        prop_assert_eq!(ConvKey::parse(&s).ok(), Some(k.clone()));
        prop_assert_eq!(json_roundtrip(&k)?, k);
    }

    #[test]
    fn net_msg_id_grammar_roundtrips(
        local in local_part(),
        author in prop::option::of(user_id()),
    ) {
        let id = NetMsgId { author, local: local.into() };
        prop_assert_eq!(id.validate(), Ok(()));
        let s = id.encode();
        prop_assert!(s.starts_with("m1:"));
        prop_assert_eq!(NetMsgId::parse(&s).ok(), Some(id.clone()));
        prop_assert_eq!(json_roundtrip(&id)?, id);
    }

    /// Parsers never panic, and whatever they accept is in canonical form.
    #[test]
    fn parsers_are_total_and_canonical(
        s in prop_oneof![
            any::<String>(),
            ("(direct:|group:|broadcast:|channel:|m1:|)", "[a-z0-9:@% +-]{0,24}")
                .prop_map(|(p, r)| format!("{p}{r}")),
        ]
    ) {
        if let Ok(u) = UserId::parse(&s) {
            prop_assert_eq!(u.as_str(), s.as_str());
        }
        if let Ok(k) = ConvKey::parse(&s) {
            prop_assert_eq!(k.encode(), s.clone());
        }
        if let Ok(id) = NetMsgId::parse(&s) {
            prop_assert_eq!(id.encode(), s.clone());
        }
    }
}

#[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
struct Doc {
    #[serde(default, skip_serializing_if = "Patch::is_unchanged")]
    field: Patch<i64>,
}

proptest! {
    /// `Patch` is exactly `Option<Option<T>>`: unchanged | clear | set, and the three states
    /// survive serialization inside a patch struct.
    #[test]
    fn patch_has_option_option_semantics(
        patch in prop::option::of(prop::option::of(any::<i64>())),
        current in prop::option::of(any::<i64>()),
    ) {
        let p = Patch::from(patch);
        let want = match patch {
            None => current,
            Some(v) => v,
        };
        prop_assert_eq!(p.applied(current), want);
        let mut field = current;
        prop_assert_eq!(p.apply(&mut field), patch.is_some());
        prop_assert_eq!(field, want);
        prop_assert_eq!(Option::<Option<i64>>::from(p), patch);
        let doc = Doc { field: p };
        let json = serde_json::to_string(&doc).expect("serialize");
        prop_assert_eq!(json == "{}", patch.is_none());
        prop_assert_eq!(serde_json::from_str::<Doc>(&json).ok(), Some(doc));
    }

    #[test]
    fn redacted_never_prints_content(s in any::<String>(), n in any::<i64>()) {
        let text = Redacted::new(format!("SECRET{s}"));
        let num = Redacted::new(n);
        for out in [
            format!("{text}"),
            format!("{text:?}"),
            format!("{text:#?}"),
            format!("{:?}", Some(&text)),
            format!("{:?}", (&text, &num)),
        ] {
            prop_assert!(!out.contains("SECRET"), "{}", out);
            prop_assert!(out.contains("[redacted]"));
        }
        prop_assert_eq!(format!("{num:?} {num}"), "[redacted] [redacted]");
    }
}
