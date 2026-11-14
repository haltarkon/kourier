use super::lists::*;
use super::*;

fn glyphs_of(found: &[u16]) -> Vec<&'static str> {
    found.iter().map(|&i| glyph(usize::from(i))).collect()
}

#[test]
fn the_table_has_the_set_up_to_the_cap_and_no_variants() {
    assert_eq!(EMOJI_VERSION_CAP, "16.0");
    assert!((1800..2000).contains(&COUNT), "{COUNT}");
    assert_eq!(group_count(), 9);
    assert_eq!(group_range(0).start, 0);
    assert_eq!(group_range(group_count() - 1).end, COUNT);
    for g in 1..group_count() {
        assert_eq!(group_range(g - 1).end, group_range(g).start);
    }
    assert!(!group_key(0).is_empty());
    for i in 0..COUNT {
        let g = glyph(i);
        assert!(!g.is_empty() && !name(i, Lang::En).is_empty() && !name(i, Lang::Uk).is_empty());
        for c in g.chars() {
            let c = c as u32;
            assert!(!(0x1F3FB..=0x1F3FF).contains(&c), "tone in {g}");
            assert!(!(0x1F9B0..=0x1F9B3).contains(&c), "hair in {g}");
        }
    }
    // The newest single emoji the font does not draw (Emoji 17.0 and 18.0) is not in.
    assert!(!is_known("\u{1FAEA}") && !is_known("\u{1FAEB}"));
    assert!(is_known("\u{1FAE9}") && is_known("❤️") && is_known("👍"));
    // No Component group: its first member is the light skin tone.
    assert!(!is_known("\u{1F3FB}"));
}

#[test]
fn a_glyph_is_found_by_its_place_and_only_the_table_is_known() {
    for i in 0..COUNT {
        assert_eq!(index_of(glyph(i)), Some(i), "{}", glyph(i));
    }
    for unknown in ["", "x", "😀😀", "\u{1F3FB}", "❤"] {
        assert_eq!(index_of(unknown), None, "{unknown:?}");
    }
}

#[test]
fn both_languages_are_present() {
    let uk = (0..COUNT)
        .filter(|&i| name(i, Lang::Uk) != name(i, Lang::En))
        .count();
    assert!(
        uk * 100 >= COUNT * 95,
        "{uk} of {COUNT} have a Ukrainian name"
    );
    let i = index_of("😀").expect("grinning");
    assert_eq!(name(i, Lang::En), "grinning face");
    assert!(name(i, Lang::Uk).contains("обличчя"));
}

#[test]
fn an_english_query_finds_by_name_keyword_and_word_start() {
    let found = glyphs_of(&search("grinning", Lang::En));
    assert!(found.contains(&"😀"));
    // A prefix of a word matches; every word must.
    let prefix = glyphs_of(&search("grinn", Lang::En));
    assert!(prefix.contains(&"😀"));
    let two = glyphs_of(&search("grin eyes", Lang::En));
    assert!(two.contains(&"😄") && !two.contains(&"🐶"));
    // A keyword, not the name: a dog's "pet".
    assert!(glyphs_of(&search("pet", Lang::En)).contains(&"🐶"));
    // The middle of a word is no match.
    assert!(!glyphs_of(&search("rinning", Lang::En)).contains(&"😀"));
    assert!(search("", Lang::En).is_empty());
    assert!(search("  ", Lang::En).is_empty());
    assert!(search("qqqqzzzz", Lang::En).is_empty());
}

#[test]
fn a_ukrainian_query_finds_the_same_emoji() {
    let found = glyphs_of(&search("усміх", Lang::Uk));
    assert!(found.contains(&"😀"), "{found:?}");
    // Ukrainian words found while the interface is English too.
    assert!(glyphs_of(&search("усміх", Lang::En)).contains(&"😀"));
    // The Ukrainian apostrophe is no part of a word: "м'яч" and "мяч" and "м’яч" are one.
    let a = search("м'яч", Lang::Uk);
    assert!(!a.is_empty());
    assert_eq!(a, search("мяч", Lang::Uk));
    assert_eq!(a, search("м’яч", Lang::Uk));
}

#[test]
fn case_and_accents_do_not_count() {
    let plain = search("pinata", Lang::En);
    assert!(glyphs_of(&plain).contains(&"🪅"), "{:?}", glyphs_of(&plain));
    assert_eq!(plain, search("PIÑATA", Lang::En));
    assert_eq!(plain, search("pin\u{0303}ata", Lang::En));
    assert_eq!(search("GRINNING", Lang::En), search("grinning", Lang::En));
    assert_eq!(fold("Łódź ß Ёж"), "lodz ss еж");
}

#[test]
fn the_interface_language_comes_first_then_the_rank_then_the_table_order() {
    let mut split_seen = false;
    let mut ranks_seen = [false; 3];
    for q in ["ok", "a", "ко", "ма", "sm", "heart", "сер"] {
        for ui in [Lang::En, Lang::Uk] {
            let found = search(q, ui);
            let words = words(q);
            let fields = |i: u16, own: bool| {
                let r = row(usize::from(i)).unwrap();
                if (ui == Lang::En) == own {
                    (r.name_en, r.search_en)
                } else {
                    (r.name_uk, r.search_uk)
                }
            };
            let own = |i: u16| all_prefixes(fields(i, true).1, &words);
            // Rank, then the table's place: strictly growing within a part.
            let key = |i: u16, own: bool| {
                let (name, s) = fields(i, own);
                (rank(name, s, &words), i)
            };
            let first_other = found.iter().position(|&i| !own(i));
            let p = first_other.unwrap_or(found.len());
            // From the first hit of the other language on, no hit of the own language.
            assert!(found[p..].iter().all(|&i| !own(i)), "{q} {ui:?}");
            split_seen |= p > 0 && p < found.len();
            assert!(
                found[..p]
                    .windows(2)
                    .all(|w| key(w[0], true) < key(w[1], true)),
                "{q} {ui:?}"
            );
            assert!(
                found[p..]
                    .windows(2)
                    .all(|w| key(w[0], false) < key(w[1], false)),
                "{q} {ui:?}"
            );
            for &i in &found[..p] {
                ranks_seen[usize::from(key(i, true).0)] = true;
            }
        }
    }
    assert!(split_seen, "some query must have hits in both languages");
    assert_eq!(ranks_seen, [true; 3], "every rank must be met");
    // The same word in both interface languages: the first hit differs where only one language knows it.
    let en = search("ко", Lang::En);
    let uk = search("ко", Lang::Uk);
    assert_eq!(en.len(), uk.len());
}

/// His answer of 2026-11-12, "Name matches first": the hearts before the winking face that
/// "heart" finds only through its keyword "heartbreaker".
#[test]
fn a_name_match_comes_before_a_keyword_word_start() {
    let found = glyphs_of(&search("heart", Lang::En));
    let at = |g: &str| found.iter().position(|f| *f == g).expect(g);
    assert!(at("❤️") < at("😉"), "{found:?}");
    assert!(at("💔") < at("😉"), "{found:?}");
    let first = search("heart", Lang::En)[0];
    assert!(
        words(name(usize::from(first), Lang::En))
            .iter()
            .any(|w| w.starts_with("heart")),
        "{found:?}"
    );
    // The same in Ukrainian: «серце» names the red heart («червоне серце»).
    let uk = search("серце", Lang::Uk);
    assert!(
        words(name(usize::from(uk[0]), Lang::Uk))
            .iter()
            .any(|w| w.starts_with("серце")),
        "{:?}",
        glyphs_of(&uk)
    );
    let red = index_of("❤️").expect("red heart");
    assert_eq!(rank(name(red, Lang::Uk), "", &words("серце")), 0);
}

#[test]
fn an_emoji_typed_as_the_query_is_found_first() {
    let found = glyphs_of(&search("🐶", Lang::En));
    assert_eq!(found.first(), Some(&"🐶"));
}

#[test]
fn a_search_over_the_whole_set_is_fast() {
    // A debug build with an optimiser of zero is slow: the 10 ms line is measured in release by
    // `cargo xtask perf`'s notes; here only that a keystroke is far from a second.
    let t = std::time::Instant::now();
    for q in ["g", "gr", "grin", "усм", "flag", "ко"] {
        std::hint::black_box(search(q, Lang::Uk));
    }
    assert!(
        t.elapsed() < std::time::Duration::from_secs(1),
        "{:?}",
        t.elapsed()
    );
}

fn first_known(n: usize) -> Vec<String> {
    (0..n).map(|i| glyph(i).to_owned()).collect()
}

#[test]
fn the_default_pins_are_signals_six_in_its_order() {
    let pins = default_pins();
    assert_eq!(pins, ["❤️", "👍", "👎", "😂", "😮", "😢"]);
    assert!(pins.iter().all(|p| is_known(p)));
    assert_eq!(hover_pins(&pins).len(), 3);
    assert_eq!(hover_pins(&pins), ["❤️", "👍", "👎"]);
    assert_eq!(hover_pins(&pins[..2]).len(), 2);
    assert!(hover_pins(&[]).is_empty());
}

#[test]
fn recent_is_newest_first_once_each_and_at_most_24() {
    let mut recent = Vec::new();
    for e in first_known(30) {
        assert!(touch_recent(&mut recent, &e));
    }
    assert_eq!(recent.len(), RECENT_MAX);
    assert_eq!(recent[0], glyph(29));
    assert_eq!(recent[23], glyph(6));
    // Using one that is in the list moves it to the front, the length stays.
    let old = recent[10].clone();
    assert!(touch_recent(&mut recent, &old));
    assert_eq!(recent[0], old);
    assert_eq!(recent.len(), RECENT_MAX);
    assert_eq!(recent.iter().filter(|e| **e == old).count(), 1);
    // The front one again is no change; what is no emoji of the table is ignored.
    assert!(!touch_recent(&mut recent, &old));
    assert!(!touch_recent(&mut recent, "x"));
    assert!(!touch_recent(&mut recent, "\u{1F3FB}"));
}

#[test]
fn pins_take_six_and_the_last_one_can_go() {
    let mut pins = Vec::new();
    for e in first_known(6) {
        assert!(toggle_pin(&mut pins, &e));
    }
    assert!(!toggle_pin(&mut pins, glyph(7)), "a seventh is refused");
    assert_eq!(pins.len(), 6);
    assert!(toggle_pin(&mut pins, glyph(0)));
    assert_eq!(pins.len(), 5);
    assert!(toggle_pin(&mut pins, glyph(7)));
    assert_eq!(pins.last().map(String::as_str), Some(glyph(7)));
    for e in pins.clone() {
        assert!(toggle_pin(&mut pins, &e));
    }
    assert!(pins.is_empty());
    assert!(!toggle_pin(&mut pins, "x"));
}

#[test]
fn favourites_toggle_and_clean_keeps_only_the_table() {
    let mut fav = Vec::new();
    assert!(toggle_favourite(&mut fav, "😀"));
    assert!(toggle_favourite(&mut fav, "🐶"));
    assert!(toggle_favourite(&mut fav, "😀"));
    assert_eq!(fav, ["🐶"]);
    assert!(!toggle_favourite(&mut fav, "nope"));
    let dirty: Vec<String> = ["🐶", "x", "🐶", "😀", ""].map(String::from).to_vec();
    assert_eq!(clean(&dirty, 10), ["🐶", "😀"]);
    assert_eq!(clean(&dirty, 1), ["🐶"]);
}

/// The time of one keystroke over the whole set, for the report (`cargo test --release -p
/// kourier-viewmodel emoji::tests::keystroke_time -- --ignored --nocapture`): the line is 10 ms.
#[test]
#[ignore = "a measurement, run by hand in a release build"]
fn keystroke_time() {
    let mut times = Vec::new();
    for word in [
        "grinning",
        "smile",
        "усміхнене",
        "прапор",
        "heart",
        "face",
        "a",
        "ко",
    ] {
        for end in word.char_indices().map(|(i, c)| i + c.len_utf8()) {
            let q = &word[..end];
            for lang in [Lang::En, Lang::Uk] {
                let t = std::time::Instant::now();
                std::hint::black_box(search(q, lang));
                times.push(t.elapsed());
            }
        }
    }
    times.sort();
    println!(
        "{} searches over {COUNT} emoji: median {:?}, p95 {:?}, max {:?}",
        times.len(),
        times[times.len() / 2],
        times[times.len() * 95 / 100],
        times[times.len() - 1]
    );
}
