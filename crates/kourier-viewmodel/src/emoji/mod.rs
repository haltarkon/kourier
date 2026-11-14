//! The emoji the pickers offer (architecture §8.3 `EmojiModel`): the generated table, the search
//! over names and keywords in English and Ukrainian, and the rules of the three lists (pins,
//! favourites, recently used). Qt-free; `kourier-qt`'s `EmojiModel` shows what it answers.
//!
//! The table is static read-only data (`table.rs`, written by `cargo xtask emoji`): nothing is
//! parsed or allocated before a picker asks, and nothing here keeps a per-emoji object.

mod fold;
pub mod lists;
mod table;

pub use fold::{fold, words};
pub use table::{COUNT, EMOJI_VERSION_CAP};

/// The two languages the table names emoji in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    En,
    Uk,
}

impl Lang {
    /// The language of a code the UI reports (`uk`, `uk_UA`, `en`, anything else is English).
    pub fn from_code(code: &str) -> Self {
        if code.trim().to_ascii_lowercase().starts_with("uk") {
            Self::Uk
        } else {
            Self::En
        }
    }

    /// The UI's language, asked of the translator: the catalog turns "en" into its own code.
    pub fn ui() -> Self {
        Self::from_code(&crate::i18n::trc(
            "@info:meta language code of the interface, used to name emoji",
            "en",
        ))
    }
}

/// One row of the table.
#[derive(Clone, Copy)]
struct Row {
    glyph: &'static str,
    name_en: &'static str,
    name_uk: &'static str,
    search_en: &'static str,
    search_uk: &'static str,
}

fn row(i: usize) -> Option<Row> {
    let start = *table::OFFS.get(i)? as usize;
    let end = *table::OFFS.get(i + 1)? as usize;
    let text = table::BLOB.get(start..end)?.trim_end_matches('\n');
    let mut f = text.split('\t');
    Some(Row {
        glyph: f.next()?,
        name_en: f.next()?,
        name_uk: f.next()?,
        search_en: f.next()?,
        search_uk: f.next()?,
    })
}

/// The glyph of emoji `i` (the table's order).
pub fn glyph(i: usize) -> &'static str {
    row(i).map_or("", |r| r.glyph)
}

/// The name of emoji `i` in `lang` (CLDR; English when CLDR has no Ukrainian name).
pub fn name(i: usize, lang: Lang) -> &'static str {
    row(i).map_or("", |r| match lang {
        Lang::En => r.name_en,
        Lang::Uk => r.name_uk,
    })
}

/// The position of `glyph` in the table (fully-qualified form).
pub fn index_of(glyph: &str) -> Option<usize> {
    // Binary search over the glyph-ordered positions: a dozen rows are read, not the whole table.
    let at = table::BY_GLYPH
        .binary_search_by(|&i| {
            row(usize::from(i)).map_or(std::cmp::Ordering::Less, |r| r.glyph.cmp(glyph))
        })
        .ok()?;
    Some(usize::from(*table::BY_GLYPH.get(at)?))
}

/// Whether `glyph` is an emoji of the table.
pub fn is_known(glyph: &str) -> bool {
    index_of(glyph).is_some()
}

/// How many groups the table has.
pub fn group_count() -> usize {
    table::GROUPS.len()
}

/// The Unicode name of group `g` (a stable key, English: the UI translates it).
pub fn group_key(g: usize) -> &'static str {
    table::GROUPS.get(g).map_or("", |g| g.0)
}

/// The emoji of group `g`, as positions in the table.
pub fn group_range(g: usize) -> std::ops::Range<usize> {
    let Some(first) = table::GROUPS.get(g) else {
        return 0..0;
    };
    let end = table::GROUPS
        .get(g + 1)
        .map_or(COUNT, |next| usize::from(next.1));
    usize::from(first.1)..end
}

/// Whether every query word is the start of one of `search`'s words.
fn all_prefixes(search: &str, query: &[String]) -> bool {
    query
        .iter()
        .all(|q| search.split(' ').any(|w| w.starts_with(q.as_str())))
}

/// How well a hit matched, best first: every query word starts a word of the emoji's NAME (0),
/// every query word is a whole search word (1), every query word only starts a search word (2).
/// His answer of 2026-11-12, "Name matches first": "heart" lists ❤️ (red heart) before 😉,
/// which "heart" finds only through its keyword "heartbreaker".
fn rank(name: &str, search: &str, query: &[String]) -> u8 {
    let name = words(name);
    if query
        .iter()
        .all(|q| name.iter().any(|w| w.starts_with(q.as_str())))
    {
        0
    } else if query.iter().all(|q| search.split(' ').any(|w| w == q)) {
        1
    } else {
        2
    }
}

/// The emoji a query finds, as positions in the table: those found by the words of the UI's
/// language first, then those only the other language's words found; each part by [`rank`],
/// then in the table's order. A query word matches when it is the start of a word of a name or
/// a keyword; every word must match; case and accents do not count. An emoji typed or pasted
/// as the query is found first. An empty query finds nothing.
pub fn search(query: &str, ui: Lang) -> Vec<u16> {
    let q = words(query);
    let mut typed = Vec::new();
    let mut mine = Vec::new();
    let mut other = Vec::new();
    let trimmed = query.trim();
    for i in 0..COUNT {
        let Some(r) = row(i) else { continue };
        let Ok(at) = u16::try_from(i) else { break };
        if !trimmed.is_empty() && r.glyph == trimmed {
            typed.push(at);
            continue;
        }
        if q.is_empty() {
            continue;
        }
        let (own, own_name, rest, rest_name) = match ui {
            Lang::En => (r.search_en, r.name_en, r.search_uk, r.name_uk),
            Lang::Uk => (r.search_uk, r.name_uk, r.search_en, r.name_en),
        };
        if all_prefixes(own, &q) {
            mine.push((rank(own_name, own, &q), at));
        } else if all_prefixes(rest, &q) {
            other.push((rank(rest_name, rest, &q), at));
        }
    }
    // A stable sort: the table's order stays within a rank.
    mine.sort_by_key(|&(r, _)| r);
    other.sort_by_key(|&(r, _)| r);
    typed.extend(mine.into_iter().map(|(_, at)| at));
    typed.extend(other.into_iter().map(|(_, at)| at));
    typed
}

#[cfg(test)]
mod tests;
