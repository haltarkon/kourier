//! RegExp filters: the saved patterns and the matcher (notes/kourier/regexp-filters.md).
//!
//! Pure: no database, no clock, no thread. The store lives in `filter_store.rs`, the commands in
//! `runtime.rs`; a front-end reaches this module as `facade::filters`.
//!
//! A [`Filter`] has up to three patterns, one per field it looks at (the message's text, the
//! sender's name, the chat's name). ALL the filled fields must match. A [`FilterSet`] compiles
//! every valid filter once: per field one `RegexSet` of only the filters that use the field
//! (which filters match), plus each filter's own `Regex` (where, for the marks). The set is
//! consumer-neutral: it holds every valid filter whatever its switches, and
//! [`FilterSet::matches`] says which match; the consumer (notify, highlight, later others)
//! keeps those whose own switch is on.
//!
//! Patterns are Rust `regex` syntax (no look-around, no back-references), Unicode on, `^` and
//! `$` match at every line, case ignored unless the filter says `match_case`.
//!
//! Privacy: a pattern, and a filter's name, is never logged. Patterns and names are
//! [`Redacted`] in the types; a `regex::Error` quotes its pattern in `Display`, so no error of
//! this module keeps one: [`PatternError`] is a kind and a position only.

use std::fmt;
use std::ops::Range;

use kourier_model::Redacted;
use regex::{Regex, RegexBuilder, RegexSet, RegexSetBuilder};
use regex_automata::meta;
use regex_automata::nfa::thompson::WhichCaptures;
use regex_automata::util::syntax;

/// The most characters (Unicode scalar values) of one pattern.
pub const MAX_PATTERN_CHARS: usize = 10_000;
/// The most characters of a filter's name.
pub const MAX_NAME_CHARS: usize = 64;
/// The compiled-size limit of one pattern (`regex`'s `size_limit`): a pattern over it is
/// refused as too large. 1 MiB (R6, release, measured on the server): `\w{20}` compiles (a
/// Unicode `\w` is about 45 KiB, so 256 KiB refused `\w{6}`) in 7.9 ms, a cost the dialog's
/// check on each key can afford (`filter_timing_too_large_cutoff`).
pub const SIZE_LIMIT: usize = 1024 * 1024;
/// The lazy DFA's cache limit of one compiled regex.
pub const DFA_SIZE_LIMIT: usize = 2 * 1024 * 1024;
/// The lazy DFA's cache limit of a field's `RegexSet`, however many filters it holds.
pub const SET_DFA_SIZE_LIMIT: usize = 8 * 1024 * 1024;
/// The most spans [`FilterSet::spans`] returns for one text.
pub const MAX_SPANS: usize = 1_000;
/// The highest colour value (`0xRRGGBB`).
pub const MAX_COLOUR: u32 = 0x00FF_FFFF;

/// A saved filter's id (core.db `filters.id`, never reused while the row exists).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FilterId(pub i64);

/// What a filter looks at.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Field {
    /// The message's text as shown.
    Text,
    /// The sender's name as shown above the bubble.
    Sender,
    /// The chat's name (its title in the list).
    Chat,
}

impl Field {
    /// The three fields, in the order of the columns.
    pub const ALL: [Self; 3] = [Self::Text, Self::Sender, Self::Chat];

    fn index(self) -> usize {
        match self {
            Self::Text => 0,
            Self::Sender => 1,
            Self::Chat => 2,
        }
    }
}

/// What the user typed for a filter. Build it with `..Default::default()`: a later consumer
/// adds one more independent switch and no caller is broken (so no `#[non_exhaustive]`, which
/// would forbid building it in the viewmodel).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FilterSpec {
    /// The filter's name (1 to [`MAX_NAME_CHARS`] characters, trimmed).
    pub name: Redacted<String>,
    /// The pattern for the message's text; `None` (or empty): not used.
    pub text_pattern: Option<Redacted<String>>,
    /// The pattern for the sender's name.
    pub sender_pattern: Option<Redacted<String>>,
    /// The pattern for the chat's name.
    pub chat_pattern: Option<Redacted<String>>,
    /// Case matters (off: case is ignored).
    pub match_case: bool,
    /// A message that matches notifies.
    pub notify: bool,
    /// The notification also comes from a muted chat (only with `notify`).
    pub notify_muted: bool,
    /// A match is marked in the chat.
    pub highlight: bool,
    /// The marker colour, `0xRRGGBB`.
    pub colour: u32,
}

impl FilterSpec {
    /// The pattern of `field`, if the filter uses it.
    pub fn pattern(&self, field: Field) -> Option<&str> {
        match field {
            Field::Text => &self.text_pattern,
            Field::Sender => &self.sender_pattern,
            Field::Chat => &self.chat_pattern,
        }
        .as_ref()
        .map(|p| p.expose().as_str())
        .filter(|p| !p.is_empty())
    }

    /// How many fields the filter uses.
    pub fn fields_used(&self) -> usize {
        Field::ALL
            .into_iter()
            .filter(|f| self.pattern(*f).is_some())
            .count()
    }
}

/// A saved filter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Filter {
    /// Its id.
    pub id: FilterId,
    /// What the user typed.
    pub spec: FilterSpec,
}

/// What a pattern's syntax error is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PatternErrorKind {
    /// A group `(` is not closed.
    UnclosedGroup,
    /// A class `[` is not closed.
    UnclosedClass,
    /// A backslash escape that means nothing.
    UnknownEscape,
    /// A repetition (`*`, `+`, `?`, `{n}`) with nothing before it.
    NothingToRepeat,
    /// Look-around (`(?=`, `(?!`, `(?<=`, `(?<!`) is not supported.
    LookAroundUnsupported,
    /// Back-references (`\1`) are not supported.
    BackReferenceUnsupported,
    /// Too large or too deeply nested to compile.
    TooLarge,
    /// Anything else the syntax refuses.
    Other,
}

/// What is wrong with a pattern, and where: never the pattern itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PatternError {
    /// What.
    pub kind: PatternErrorKind,
    /// Where, as a UTF-16 index into the pattern (what a QML `TextField` counts); 0 for what
    /// only the compile knows (too large).
    pub at_utf16: usize,
}

fn utf16_of_byte(text: &str, byte: usize) -> usize {
    let byte = byte.min(text.len());
    text.char_indices()
        .take_while(|(i, _)| *i < byte)
        .map(|(_, c)| c.len_utf16())
        .sum()
}

/// The syntax check for a dialog or a search field: parses only (microseconds for a few
/// hundred characters, fine on the UI thread). The size limit is known only at compile, so a
/// pattern that parses can still be refused as too large when it is saved.
pub fn check(pattern: &str) -> Result<(), PatternError> {
    use regex_syntax::ast::ErrorKind as A;
    let err = match regex_syntax::ParserBuilder::new().build().parse(pattern) {
        Ok(_) => return Ok(()),
        Err(e) => e,
    };
    let (kind, byte) = match &err {
        regex_syntax::Error::Parse(e) => (
            match e.kind() {
                A::GroupUnclosed => PatternErrorKind::UnclosedGroup,
                A::ClassUnclosed => PatternErrorKind::UnclosedClass,
                A::EscapeUnrecognized => PatternErrorKind::UnknownEscape,
                A::RepetitionMissing => PatternErrorKind::NothingToRepeat,
                A::UnsupportedLookAround => PatternErrorKind::LookAroundUnsupported,
                A::UnsupportedBackreference => PatternErrorKind::BackReferenceUnsupported,
                A::NestLimitExceeded(_) | A::CaptureLimitExceeded => PatternErrorKind::TooLarge,
                _ => PatternErrorKind::Other,
            },
            e.span().start.offset,
        ),
        regex_syntax::Error::Translate(e) => (PatternErrorKind::Other, e.span().start.offset),
        _ => (PatternErrorKind::Other, 0),
    };
    Err(PatternError {
        kind,
        at_utf16: utf16_of_byte(pattern, byte),
    })
}

/// Why a filter is refused (a save). Log-safe: no pattern, no name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FilterRefusal {
    /// The name is empty.
    NameEmpty,
    /// The name is longer than [`MAX_NAME_CHARS`].
    NameTooLong,
    /// No field has a pattern.
    NoField,
    /// The colour is not `0xRRGGBB`.
    ColourOutOfRange,
    /// A pattern is longer than [`MAX_PATTERN_CHARS`].
    PatternTooLong(Field),
    /// A pattern does not compile (its syntax, or too large).
    Pattern(Field, PatternError),
    /// There is no filter with that id.
    Unknown,
}

impl fmt::Display for FilterRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Fixed words and the kind only: never what the user typed.
        match self {
            Self::NameEmpty => f.write_str("the filter has no name"),
            Self::NameTooLong => f.write_str("the filter's name is too long"),
            Self::NoField => f.write_str("the filter uses no field"),
            Self::ColourOutOfRange => f.write_str("the filter's colour is out of range"),
            Self::PatternTooLong(field) => write!(f, "a pattern is too long ({field:?})"),
            Self::Pattern(field, e) => write!(f, "a pattern is refused ({field:?}: {:?})", e.kind),
            Self::Unknown => f.write_str("no such filter"),
        }
    }
}

fn builder(pattern: &str, match_case: bool) -> RegexBuilder {
    let mut b = RegexBuilder::new(pattern);
    b.case_insensitive(!match_case)
        .multi_line(true)
        .unicode(true)
        .size_limit(SIZE_LIMIT)
        .dfa_size_limit(DFA_SIZE_LIMIT);
    b
}

/// Compiles one pattern the way a [`FilterSet`] does. The error carries no pattern.
pub fn compile(pattern: &str, match_case: bool) -> Result<Regex, PatternError> {
    check(pattern)?;
    builder(pattern, match_case)
        .build()
        .map_err(|e| PatternError {
            kind: match e {
                regex::Error::CompiledTooBig(_) => PatternErrorKind::TooLarge,
                _ => PatternErrorKind::Other,
            },
            at_utf16: 0,
        })
}

/// A filter's own regex, as `meta::Regex` configured for what the set uses it for: the whole
/// match only (`WhichCaptures::Implicit`: no capture slots, so the engines that need them, the
/// one-pass DFA among them, are not built), the same syntax flags, size and DFA limits as
/// [`compile`]. The matches and spans are the same (R6b).
fn compile_own(pattern: &str, match_case: bool) -> Result<meta::Regex, PatternError> {
    check(pattern)?;
    meta::Builder::new()
        .configure(
            meta::Config::new()
                .which_captures(WhichCaptures::Implicit)
                .onepass(false)
                .nfa_size_limit(Some(SIZE_LIMIT))
                .hybrid_cache_capacity(DFA_SIZE_LIMIT),
        )
        .syntax(
            syntax::Config::new()
                .case_insensitive(!match_case)
                .multi_line(true)
                .unicode(true),
        )
        .build(pattern)
        .map_err(|e| PatternError {
            kind: if e.size_limit().is_some() {
                PatternErrorKind::TooLarge
            } else {
                PatternErrorKind::Other
            },
            at_utf16: 0,
        })
}

/// `pattern` with its Unicode word boundaries (`\b`, `\B` and their start, end and half
/// forms) taken out, for a field's `RegexSet` (R6). regex's lazy DFA cannot follow a Unicode word
/// boundary: it gives up at the first non-ASCII byte, and a set that holds ONE such pattern then
/// runs the slow engine over every pattern of the set at once (measured: 1.15 ms a text with 50
/// filters over Latin and Cyrillic text, 22 ns without filters). Taking an assertion out only
/// widens what matches, so the set stays fast and every hit of such a pattern is confirmed by
/// the filter's own regex. `None`: nothing to take out (the set's answer is exact), or it does
/// not parse (the set then holds the pattern as written).
fn without_unicode_boundaries(pattern: &str, match_case: bool) -> Option<String> {
    use regex_syntax::hir::{Capture, Hir, HirKind, Look, Repetition};
    fn strip(h: &Hir, changed: &mut bool) -> Hir {
        match h.kind() {
            HirKind::Look(
                Look::WordUnicode
                | Look::WordUnicodeNegate
                | Look::WordStartUnicode
                | Look::WordEndUnicode
                | Look::WordStartHalfUnicode
                | Look::WordEndHalfUnicode,
            ) => {
                *changed = true;
                Hir::empty()
            }
            HirKind::Repetition(r) => Hir::repetition(Repetition {
                min: r.min,
                max: r.max,
                greedy: r.greedy,
                sub: Box::new(strip(&r.sub, changed)),
            }),
            HirKind::Capture(c) => Hir::capture(Capture {
                index: c.index,
                name: c.name.clone(),
                sub: Box::new(strip(&c.sub, changed)),
            }),
            HirKind::Concat(subs) => Hir::concat(subs.iter().map(|x| strip(x, changed)).collect()),
            HirKind::Alternation(subs) => {
                Hir::alternation(subs.iter().map(|x| strip(x, changed)).collect())
            }
            _ => h.clone(),
        }
    }
    let hir = regex_syntax::ParserBuilder::new()
        .case_insensitive(!match_case)
        .multi_line(true)
        .unicode(true)
        .build()
        .parse(pattern)
        .ok()?;
    let mut changed = false;
    let out = strip(&hir, &mut changed);
    changed.then(|| out.to_string())
}

/// Checks a spec as the core does at every save: the name, at least one field, the colour and
/// each pattern (length, syntax, compiled size). An empty pattern counts as unused. Returns
/// the spec as it is stored (name trimmed, empty patterns removed).
pub fn validate(spec: &FilterSpec) -> Result<FilterSpec, FilterRefusal> {
    let name = spec.name.expose().trim();
    if name.is_empty() {
        return Err(FilterRefusal::NameEmpty);
    }
    if name.chars().count() > MAX_NAME_CHARS {
        return Err(FilterRefusal::NameTooLong);
    }
    if spec.colour > MAX_COLOUR {
        return Err(FilterRefusal::ColourOutOfRange);
    }
    let mut out = FilterSpec {
        name: Redacted::new(name.to_owned()),
        text_pattern: None,
        sender_pattern: None,
        chat_pattern: None,
        ..spec.clone()
    };
    let mut any = false;
    for field in Field::ALL {
        let Some(p) = spec.pattern(field) else {
            continue;
        };
        if p.chars().count() > MAX_PATTERN_CHARS {
            return Err(FilterRefusal::PatternTooLong(field));
        }
        compile(p, spec.match_case).map_err(|e| FilterRefusal::Pattern(field, e))?;
        any = true;
        let kept = Some(Redacted::new(p.to_owned()));
        match field {
            Field::Text => out.text_pattern = kept,
            Field::Sender => out.sender_pattern = kept,
            Field::Chat => out.chat_pattern = kept,
        }
    }
    if !any {
        return Err(FilterRefusal::NoField);
    }
    Ok(out)
}

/// The texts a message offers the filters. `None`: the text is not there (a chat's name when
/// only a message is being looked at); a filter that uses a field whose text is `None` does
/// not match.
#[derive(Clone, Copy, Debug, Default)]
pub struct FilterTexts<'a> {
    /// The message's text as shown.
    pub text: Option<&'a str>,
    /// The sender's name as shown.
    pub sender: Option<&'a str>,
    /// The chat's name.
    pub chat: Option<&'a str>,
}

/// [`FilterTexts`] with owned strings, for a caller that makes the texts in one place and
/// matches in another (the desktop's notification path). Never logged.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OwnedFilterTexts {
    /// The message's text as shown.
    pub text: Option<String>,
    /// The sender's name as shown, without the "~".
    pub sender: Option<String>,
    /// The chat's title.
    pub chat: String,
}

impl OwnedFilterTexts {
    /// The borrowed form the matcher takes.
    pub fn as_texts(&self) -> FilterTexts<'_> {
        FilterTexts {
            text: self.text.as_deref(),
            sender: self.sender.as_deref(),
            chat: Some(self.chat.as_str()),
        }
    }
}

impl<'a> FilterTexts<'a> {
    fn of(&self, field: Field) -> Option<&'a str> {
        match field {
            Field::Text => self.text,
            Field::Sender => self.sender,
            Field::Chat => self.chat,
        }
    }
}

/// Some of one field's patterns, those that share a case rule (a `RegexSet` takes one case
/// flag, and wrapping each pattern in `(?i:…)` would break a pattern that ends in an `(?x)`
/// comment), and which filter each belongs to.
struct CaseSet {
    /// `None`: the set did not build (together the patterns crossed a size limit, say); the
    /// filters' own regexes answer then.
    set: Option<RegexSet>,
    /// Per pattern of the set, its filter (index into [`FilterSet::filters`]), in list order.
    owners: Vec<usize>,
    /// Per pattern of the set: the set holds it without its Unicode word boundaries
    /// ([`without_unicode_boundaries`]), so a hit is confirmed by the filter's own regex.
    confirm: Vec<bool>,
}

/// One field's sets: at most two, case ignored and case matched.
struct FieldSet {
    parts: Vec<CaseSet>,
}

/// Test seam: milliseconds the core's start compile sleeps before it builds (0 normally), so a
/// test can send a command before the start's compile is done.
#[doc(hidden)]
pub static START_COMPILE_DELAY_MS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

pub(crate) fn start_delay() {
    let ms = START_COMPILE_DELAY_MS.load(std::sync::atomic::Ordering::SeqCst);
    if ms > 0 {
        std::thread::sleep(std::time::Duration::from_millis(ms));
    }
}

fn build_field_sets(filters: &[Filter]) -> [FieldSet; 3] {
    Field::ALL.map(|field| {
        let mut parts = Vec::new();
        for match_case in [false, true] {
            let mut owners = Vec::new();
            let mut patterns: Vec<String> = Vec::new();
            let mut confirm = Vec::new();
            for (i, f) in filters.iter().enumerate() {
                if f.spec.match_case != match_case {
                    continue;
                }
                if let Some(p) = f.spec.pattern(field) {
                    owners.push(i);
                    match without_unicode_boundaries(p, match_case) {
                        Some(wide) => {
                            patterns.push(wide);
                            confirm.push(true);
                        }
                        None => {
                            patterns.push(p.to_owned());
                            confirm.push(false);
                        }
                    }
                }
            }
            if owners.is_empty() {
                continue;
            }
            let set = RegexSetBuilder::new(&patterns)
                .case_insensitive(!match_case)
                .multi_line(true)
                .unicode(true)
                .size_limit(SIZE_LIMIT.saturating_mul(patterns.len()))
                .dfa_size_limit(SET_DFA_SIZE_LIMIT)
                .build()
                .ok();
            parts.push(CaseSet {
                set,
                owners,
                confirm,
            });
        }
        FieldSet { parts }
    })
}

/// The compiled filters: build once per change, share by `Arc`, call from any thread.
pub struct FilterSet {
    filters: Vec<Filter>,
    /// Per filter, per field, its own regex (`None`: field not used).
    regexes: Vec<[Option<meta::Regex>; 3]>,
    /// Per filter, how many fields it uses (all must match).
    used: Vec<u8>,
    fields: [FieldSet; 3],
}

impl fmt::Debug for FilterSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Counts only: the filters are redacted anyway, the regexes are not.
        f.debug_struct("FilterSet")
            .field("filters", &self.filters.len())
            .finish()
    }
}

impl FilterSet {
    /// A set with no filter.
    pub fn empty() -> Self {
        Self::build(&[])
    }

    /// Compiles `filters` (in list order). A filter that does not compile under the limits
    /// is left out (the core validated it at its save, so this is a stored row from a
    /// version with other rules); [`Self::filters`] is what remained, and the indices
    /// [`Self::matches`] gives are into it, NOT into a list of stored rows.
    pub fn build(all: &[Filter]) -> Self {
        let mut filters = Vec::with_capacity(all.len());
        let mut regexes: Vec<[Option<meta::Regex>; 3]> = Vec::with_capacity(all.len());
        for f in all {
            let mut one: [Option<meta::Regex>; 3] = [None, None, None];
            let mut ok = f.spec.fields_used() > 0;
            for field in Field::ALL {
                if let Some(p) = f.spec.pattern(field) {
                    match compile_own(p, f.spec.match_case) {
                        Ok(r) => one[field.index()] = Some(r),
                        Err(_) => ok = false,
                    }
                }
            }
            if ok {
                filters.push(f.clone());
                regexes.push(one);
            }
        }
        let used = filters
            .iter()
            .map(|f| u8::try_from(f.spec.fields_used()).unwrap_or(u8::MAX))
            .collect();
        let fields = build_field_sets(&filters);
        Self {
            filters,
            regexes,
            used,
            fields,
        }
    }

    /// The filters the set holds, in list order.
    pub fn filters(&self) -> &[Filter] {
        &self.filters
    }

    /// How many filters.
    pub fn len(&self) -> usize {
        self.filters.len()
    }

    /// No filter at all.
    pub fn is_empty(&self) -> bool {
        self.filters.is_empty()
    }

    /// Calls `hit` with each filter (index into [`Self::filters`]) whose pattern for `field`
    /// matches `text`; one set pass per case rule.
    fn each_hit(&self, field: Field, text: &str, mut hit: impl FnMut(usize)) {
        for part in &self.fields[field.index()].parts {
            match &part.set {
                Some(set) => {
                    for i in set.matches(text).into_iter() {
                        let o = part.owners[i];
                        if !part.confirm[i]
                            || self.regexes[o][field.index()]
                                .as_ref()
                                .is_some_and(|r| r.is_match(text))
                        {
                            hit(o);
                        }
                    }
                }
                None => {
                    for &o in &part.owners {
                        if self.regexes[o][field.index()]
                            .as_ref()
                            .is_some_and(|r| r.is_match(text))
                        {
                            hit(o);
                        }
                    }
                }
            }
        }
    }

    /// The indices (into [`Self::filters`], in list order) of the filters that match: every
    /// field a filter uses must match the text given for it.
    pub fn matches(&self, texts: &FilterTexts<'_>) -> std::vec::IntoIter<usize> {
        let mut hits: Vec<u8> = Vec::new();
        for field in Field::ALL {
            let Some(text) = texts.of(field) else {
                continue;
            };
            self.each_hit(field, text, |owner| {
                if hits.is_empty() {
                    hits.resize(self.filters.len(), 0);
                }
                hits[owner] += 1;
            });
        }
        hits.iter()
            .zip(&self.used)
            .enumerate()
            .filter_map(|(i, (h, u))| (h == u && *u > 0).then_some(i))
            .collect::<Vec<_>>()
            .into_iter()
    }

    /// The indices (in list order) of the filters whose pattern for `field` alone matches
    /// `text` (the other fields are not looked at: a chat's name is marked in the list
    /// whenever its own pattern matches).
    pub fn matches_field(&self, field: Field, text: &str) -> Vec<usize> {
        let mut out = Vec::new();
        self.each_hit(field, text, |o| out.push(o));
        out.sort_unstable();
        out
    }

    /// Where filter `index` matches in `text` for `field`, as UTF-16 ranges (what a Qt text
    /// counts), in order, empty matches left out, at most [`MAX_SPANS`].
    pub fn spans(&self, index: usize, field: Field, text: &str) -> Vec<Range<usize>> {
        let Some(re) = self
            .regexes
            .get(index)
            .and_then(|r| r[field.index()].as_ref())
        else {
            return Vec::new();
        };
        let mut out = Vec::new();
        // One walk over the text turns byte offsets into UTF-16 offsets.
        let (mut byte, mut utf16) = (0usize, 0usize);
        let mut chars = text.chars();
        let mut advance = |to: usize, byte: &mut usize, utf16: &mut usize| {
            while *byte < to {
                match chars.next() {
                    Some(c) => {
                        *byte += c.len_utf8();
                        *utf16 += c.len_utf16();
                    }
                    None => break,
                }
            }
        };
        for m in re.find_iter(text) {
            if m.is_empty() {
                continue;
            }
            advance(m.start(), &mut byte, &mut utf16);
            let start = utf16;
            advance(m.end(), &mut byte, &mut utf16);
            out.push(start..utf16);
            if out.len() >= MAX_SPANS {
                break;
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(name: &str) -> FilterSpec {
        FilterSpec {
            name: Redacted::new(name.into()),
            colour: 0xFFD400,
            ..Default::default()
        }
    }

    fn with(name: &str, field: Field, p: &str) -> FilterSpec {
        let mut s = spec(name);
        let v = Some(Redacted::new(p.to_owned()));
        match field {
            Field::Text => s.text_pattern = v,
            Field::Sender => s.sender_pattern = v,
            Field::Chat => s.chat_pattern = v,
        }
        s
    }

    fn set(specs: Vec<FilterSpec>) -> FilterSet {
        let filters: Vec<Filter> = specs
            .into_iter()
            .enumerate()
            .map(|(i, spec)| Filter {
                id: FilterId(i as i64 + 1),
                spec: validate(&spec).expect("valid"),
            })
            .collect();
        FilterSet::build(&filters)
    }

    fn hits(s: &FilterSet, t: FilterTexts<'_>) -> Vec<usize> {
        s.matches(&t).collect()
    }

    fn text(t: &str) -> FilterTexts<'_> {
        FilterTexts {
            text: Some(t),
            sender: Some("Alice"),
            chat: Some("Family"),
        }
    }

    /// R6: the set holds a pattern without its Unicode word boundaries, and the filter's own
    /// regex confirms each hit, so the answers stay those of the pattern as written.
    #[test]
    fn a_unicode_word_boundary_is_confirmed_by_the_filters_own_regex() {
        assert!(without_unicode_boundaries(r"\bсад\w*", false).is_some());
        assert!(without_unicode_boundaries(r"\Bab\b{end-half}", true).is_some());
        assert_eq!(without_unicode_boundaries("сад|garden", false), None);
        assert_eq!(without_unicode_boundaries(r"(?-u:\b)ab", false), None);
        let s = set(vec![
            with("garden", Field::Text, r"\bсад\w*"),
            with("inside", Field::Text, r"\Bсад"),
            with("plain", Field::Text, "kiwi"),
            with("named", Field::Text, r"(?P<w>\bКІТ)\b"),
        ]);
        assert_eq!(hits(&s, text("у саду")), vec![0]);
        assert_eq!(hits(&s, text("посадка")), vec![1]);
        assert_eq!(hits(&s, text("kiwi посадка")), vec![1, 2]);
        // Case ignored through the rewritten pattern too.
        assert_eq!(hits(&s, text("наш кіт спить")), vec![3]);
        assert_eq!(hits(&s, text("котик кітки")), Vec::<usize>::new());
        assert_eq!(s.matches_field(Field::Text, "сад і кіт"), vec![0, 3]);
        // Matched case keeps its own set, rewritten as well.
        let mut cased = with("cased", Field::Text, r"\bКіт\b");
        cased.match_case = true;
        let s = set(vec![cased]);
        assert_eq!(hits(&s, text("Кіт")), vec![0]);
        assert_eq!(hits(&s, text("кіт")), Vec::<usize>::new());
        assert_eq!(hits(&s, text("Кітка")), Vec::<usize>::new());
    }

    #[test]
    fn check_names_the_kind_and_the_place() {
        assert_eq!(
            check("a(b"),
            Err(PatternError {
                kind: PatternErrorKind::UnclosedGroup,
                at_utf16: 1
            })
        );
        assert_eq!(
            check("[ab").unwrap_err().kind,
            PatternErrorKind::UnclosedClass
        );
        assert_eq!(
            check(r"a\q").unwrap_err().kind,
            PatternErrorKind::UnknownEscape
        );
        assert_eq!(
            check("*a").unwrap_err().kind,
            PatternErrorKind::NothingToRepeat
        );
        assert_eq!(
            check("a(?=b)").unwrap_err().kind,
            PatternErrorKind::LookAroundUnsupported
        );
        assert_eq!(
            check("a(?<!b)").unwrap_err().kind,
            PatternErrorKind::LookAroundUnsupported
        );
        assert_eq!(
            check(r"(a)\1").unwrap_err().kind,
            PatternErrorKind::BackReferenceUnsupported
        );
        assert_eq!(check(r"\d{3,}\bслово").map(|_| ()), Ok(()));
        assert_eq!(check("").map(|_| ()), Ok(()));
    }

    #[test]
    fn the_place_counts_utf16_units() {
        // Three Cyrillic letters (one unit each) and an emoji (two units) before the error.
        let e = check("абв😀(").unwrap_err();
        assert_eq!(e.kind, PatternErrorKind::UnclosedGroup);
        assert_eq!(e.at_utf16, 5);
    }

    #[test]
    fn validation_refuses_each_thing() {
        let ok = with("n", Field::Text, "x");
        assert!(validate(&ok).is_ok());
        assert_eq!(validate(&spec("n")), Err(FilterRefusal::NoField));
        let mut s = with("   ", Field::Text, "x");
        assert_eq!(validate(&s), Err(FilterRefusal::NameEmpty));
        s.name = Redacted::new("é".repeat(65));
        assert_eq!(validate(&s), Err(FilterRefusal::NameTooLong));
        s.name = Redacted::new("é".repeat(64));
        assert!(validate(&s).is_ok());
        s.colour = MAX_COLOUR + 1;
        assert_eq!(validate(&s), Err(FilterRefusal::ColourOutOfRange));
        let long = with("n", Field::Sender, &"a".repeat(MAX_PATTERN_CHARS + 1));
        assert_eq!(
            validate(&long),
            Err(FilterRefusal::PatternTooLong(Field::Sender))
        );
        // 10,000 characters that cost nothing to compile (extended mode ignores the
        // blanks) are allowed; 10,000 characters of `\w` are not: past the compiled-size
        // limit they are refused as too large, not as too long.
        let blank = format!("(?x){}a", " ".repeat(MAX_PATTERN_CHARS - 5));
        assert_eq!(blank.chars().count(), MAX_PATTERN_CHARS);
        assert!(validate(&with("n", Field::Sender, &blank)).is_ok());
        assert!(matches!(
            validate(&with(
                "n",
                Field::Sender,
                &r"\w".repeat(MAX_PATTERN_CHARS / 2)
            )),
            Err(FilterRefusal::Pattern(
                Field::Sender,
                PatternError {
                    kind: PatternErrorKind::TooLarge,
                    ..
                }
            ))
        ));
        assert!(matches!(
            validate(&with("n", Field::Chat, "(")),
            Err(FilterRefusal::Pattern(
                Field::Chat,
                PatternError {
                    kind: PatternErrorKind::UnclosedGroup,
                    ..
                }
            ))
        ));
        // Compiles in syntax but not under the size limit.
        assert!(matches!(
            validate(&with("n", Field::Text, r"\p{L}{1000}")),
            Err(FilterRefusal::Pattern(
                Field::Text,
                PatternError {
                    kind: PatternErrorKind::TooLarge,
                    ..
                }
            ))
        ));
    }

    #[test]
    fn an_empty_pattern_is_an_unused_field_and_the_name_is_trimmed() {
        let mut s = with("  n  ", Field::Text, "x");
        s.sender_pattern = Some(Redacted::new(String::new()));
        let v = validate(&s).expect("valid");
        assert_eq!(v.name.expose(), "n");
        assert!(v.sender_pattern.is_none());
        let mut only_empty = spec("n");
        only_empty.text_pattern = Some(Redacted::new(String::new()));
        assert_eq!(validate(&only_empty), Err(FilterRefusal::NoField));
    }

    #[test]
    fn all_filled_fields_must_match_and_the_order_is_the_list_order() {
        let mut both = with("both", Field::Text, "invoice");
        both.sender_pattern = Some(Redacted::new("^ali".into()));
        let s = set(vec![
            with("chat", Field::Chat, "fam"),
            both,
            with("other", Field::Text, "never"),
            with("any", Field::Text, "inv"),
        ]);
        assert_eq!(hits(&s, text("an Invoice")), vec![0, 1, 3]);
        let other_sender = FilterTexts {
            sender: Some("Bob"),
            ..text("an invoice")
        };
        assert_eq!(hits(&s, other_sender), vec![0, 3]);
        // A text that is not there: a filter that uses it does not match.
        let no_chat = FilterTexts {
            chat: None,
            ..text("invoice")
        };
        assert_eq!(hits(&s, no_chat), vec![1, 3]);
        assert!(hits(&s, FilterTexts::default()).is_empty());
        assert!(hits(&FilterSet::empty(), text("x")).is_empty());
    }

    #[test]
    fn case_is_ignored_unless_the_filter_matches_case() {
        let mut exact = with("exact", Field::Text, "Invoice");
        exact.match_case = true;
        let s = set(vec![exact, with("loose", Field::Text, "Invoice")]);
        assert_eq!(hits(&s, text("an invoice")), vec![1]);
        assert_eq!(hits(&s, text("an Invoice")), vec![0, 1]);
        // Unicode case folding, both ways.
        let s = set(vec![with("c", Field::Text, "привіт")]);
        assert_eq!(hits(&s, text("ПРИВІТ, світе")), vec![0]);
    }

    #[test]
    fn caret_and_dollar_match_at_every_line() {
        let s = set(vec![
            with("start", Field::Text, "^code:"),
            with("end", Field::Text, "done$"),
        ]);
        assert_eq!(hits(&s, text("hello\ncode: 12\nx")), vec![0]);
        assert_eq!(hits(&s, text("all done\nnext")), vec![1]);
    }

    #[test]
    fn cyrillic_word_boundaries_work() {
        let s = set(vec![with("w", Field::Text, r"\bдоставка\b")]);
        assert_eq!(hits(&s, text("Ваша Доставка завтра")), vec![0]);
        assert!(hits(&s, text("Передоставка")).is_empty());
        assert!(hits(&s, text("доставками")).is_empty());
    }

    #[test]
    fn a_pattern_with_capture_groups_gives_the_whole_match_only() {
        let s = set(vec![
            with("g", Field::Text, r"(?P<n>\d+)-(x|y)?(z)*"),
            with("h", Field::Text, r"(?i)(ab)+\b"),
        ]);
        let t = "id 12-x and 7-zz, ABAB ok";
        assert_eq!(s.spans(0, Field::Text, t), vec![3..7, 12..16]);
        assert_eq!(s.spans(1, Field::Text, t), vec![18..22]);
        let l = FilterTexts {
            text: Some(t),
            sender: None,
            chat: None,
        };
        assert_eq!(s.matches(&l).collect::<Vec<_>>(), [0, 1]);
    }

    #[test]
    fn spans_are_utf16_and_skip_empty_matches() {
        let s = set(vec![
            with("d", Field::Text, r"\d+"),
            with("e", Field::Text, "x*"),
        ]);
        // "😀" is two UTF-16 units; "ї" one.
        let t = "😀ї 12 and 345";
        assert_eq!(s.spans(0, Field::Text, t), vec![4..6, 11..14]);
        assert!(s.spans(1, Field::Text, "abc").is_empty());
        assert!(s.spans(9, Field::Text, t).is_empty());
        assert!(s.spans(0, Field::Chat, t).is_empty());
        let many = "1 ".repeat(MAX_SPANS + 50);
        assert_eq!(s.spans(0, Field::Text, &many).len(), MAX_SPANS);
    }

    #[test]
    fn a_field_alone_marks_a_chat_name() {
        let mut two = with("two", Field::Chat, "fam");
        two.text_pattern = Some(Redacted::new("zzz".into()));
        let s = set(vec![two, with("one", Field::Chat, "^fam")]);
        assert_eq!(s.matches_field(Field::Chat, "Family"), vec![0, 1]);
        assert!(s.matches_field(Field::Sender, "Family").is_empty());
    }

    #[test]
    fn mixed_case_rules_and_an_x_mode_comment_keep_the_fast_sets() {
        // An `(?x)` pattern that ends in a comment: wrapping it in a group would let the
        // comment swallow the closing parenthesis.
        let mut exact = with(
            "exact",
            Field::Text,
            "(?x) Order \\s \\d+  # the order number",
        );
        exact.match_case = true;
        let loose = with("loose", Field::Text, "parcel");
        let mut exact2 = with("exact2", Field::Text, "ETA");
        exact2.match_case = true;
        let s = set(vec![exact, loose, exact2]);
        assert!(
            s.fields[Field::Text.index()]
                .parts
                .iter()
                .all(|p| p.set.is_some()),
            "every part built its set"
        );
        assert_eq!(s.fields[Field::Text.index()].parts.len(), 2);
        assert_eq!(hits(&s, text("Order 42, PARCEL, ETA 5")), vec![0, 1, 2]);
        assert_eq!(hits(&s, text("order 42, eta")), Vec::<usize>::new());
        assert_eq!(hits(&s, text("order 42 parcel")), vec![1]);
        assert_eq!(
            s.matches_field(Field::Text, "ETA: Parcel, Order 7"),
            vec![0, 1, 2]
        );
    }

    #[test]
    fn debug_never_shows_patterns_or_names() {
        let f = Filter {
            id: FilterId(1),
            spec: with("secret-name", Field::Text, "secret-pattern"),
        };
        let shown = format!("{f:?} {:?}", set(vec![f.spec.clone()]));
        assert!(!shown.contains("secret"), "{shown}");
        let r = FilterRefusal::Pattern(Field::Text, check("secret(").unwrap_err());
        assert!(!format!("{r} {r:?}").contains("secret"));
    }
}

/// Timings of the matcher (R6): run by name in a release build, they print numbers and assert
/// nothing about speed. `cargo nextest run --release -p kourier-core -E 'test(filter_timing)'
/// --run-ignored only --no-capture`. The mix is synthetic ([`crate::testing::filter_mix`]);
/// only counts and timings are printed, never a pattern.
#[cfg(test)]
mod timing {
    use super::*;
    use crate::testing::{filter_mix, filter_mix_word};
    use std::time::Instant;

    fn filters(n: usize) -> Vec<Filter> {
        filter_mix(n)
            .into_iter()
            .enumerate()
            .map(|(i, spec)| Filter {
                id: FilterId(i as i64 + 1),
                spec: validate(&spec).expect("the mix is valid"),
            })
            .collect()
    }

    /// 10,000 texts of 20 to 400 characters, Latin and Cyrillic mixed; one in 50 holds the
    /// first filter's word.
    fn texts() -> Vec<String> {
        let mut state = 0x2545_f491_4f6c_dd1d_u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        (0..10_000)
            .map(|i| {
                let want = 20 + (next() % 381) as usize;
                let mut t = String::new();
                while t.chars().count() < want {
                    let w = filter_mix_word(100_000 + (next() % 50_000) as usize, next() % 3 == 0);
                    t.push_str(&w);
                    t.push(' ');
                }
                if i % 50 == 7 {
                    t.push_str(&filter_mix_word(0, false));
                }
                t
            })
            .collect()
    }

    fn accepted(pattern: &str) -> bool {
        compile(pattern, false).is_ok()
    }

    /// The largest `n` in `1..=hi` for which `ok(n)` holds (`ok` holds for small n).
    fn bisect(hi: usize, ok: impl Fn(usize) -> bool) -> usize {
        let (mut lo, mut hi) = (1, hi);
        while lo < hi {
            let mid = (lo + hi).div_ceil(2);
            if ok(mid) {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }
        lo
    }

    #[test]
    #[ignore = "a measurement (R6): run by name, in release, under kcap"]
    fn filter_timing_matches_per_text() {
        let texts = texts();
        for count in [0, 10, 50, 200] {
            let all = filters(count);
            let t = Instant::now();
            let set = FilterSet::build(&all);
            let build_ms = t.elapsed().as_secs_f64() * 1000.0;
            let mut hit_texts = 0usize;
            let t = Instant::now();
            for text in &texts {
                let ft = FilterTexts {
                    text: Some(text),
                    sender: Some("Anna Example"),
                    chat: Some("Family chat"),
                };
                if set.matches(&ft).next().is_some() {
                    hit_texts += 1;
                }
            }
            let ns = t.elapsed().as_nanos() as f64 / texts.len() as f64;
            println!(
                "filter_timing: filters {count} (kept {}): build {build_ms:.1} ms, {ns:.0} ns per text over {} texts, {hit_texts} texts with a hit",
                set.len(),
                texts.len()
            );
        }
    }

    fn allocated() -> usize {
        // SAFETY: mallinfo2 only reads the allocator's counters (all arenas).
        unsafe { libc::mallinfo2() }.uordblks
    }

    fn mib(bytes: usize) -> f64 {
        bytes as f64 / (1024.0 * 1024.0)
    }

    /// Bytes retained (glibc `uordblks`) by building `make`'s value and keeping it.
    fn retained<T>(make: impl FnOnce() -> T) -> (T, usize) {
        let before = allocated();
        let v = make();
        let after = allocated();
        (v, after.saturating_sub(before))
    }

    #[test]
    #[ignore = "a measurement (R6b): run by name, in release, under kcap"]
    fn filter_memory_retained() {
        let texts = texts();
        for count in [10usize, 50, 200] {
            let all = filters(count);
            let (set, built) = retained(|| FilterSet::build(&all));
            // The own regexes alone, as `build` compiles them.
            let (own, own_bytes) = retained(|| {
                let mut v = Vec::new();
                for f in &all {
                    for field in Field::ALL {
                        if let Some(p) = f.spec.pattern(field) {
                            v.push(compile_own(p, f.spec.match_case).ok());
                        }
                    }
                }
                v
            });
            // The field sets alone.
            let (sets, set_bytes) = retained(|| build_field_sets(set.filters()));
            // One pass over the texts: caches warm.
            let before = allocated();
            let mut sink = 0usize;
            for text in &texts {
                let ft = FilterTexts {
                    text: Some(text),
                    sender: Some("Anna Example"),
                    chat: Some("Family chat"),
                };
                let hits: Vec<usize> = set.matches(&ft).collect();
                for i in hits {
                    sink += set.spans(i, Field::Text, text).len();
                }
            }
            let warm = allocated().saturating_sub(before);
            println!(
                "filter_memory: filters {count} (kept {}): built {:.2} MiB; own regexes alone {:.2} MiB; field sets alone {:.2} MiB; caches after one pass {:.2} MiB (total warm {:.2} MiB) [{sink}]",
                set.len(),
                mib(built),
                mib(own_bytes),
                mib(set_bytes),
                mib(warm),
                mib(built + warm)
            );
            drop((own, sets, set));
        }
    }

    #[test]
    #[ignore = "a measurement (R6): run by name, in release, under kcap"]
    fn filter_timing_too_large_cutoff() {
        let latin = |n: usize| {
            (0..n)
                .map(|k| filter_mix_word(k, false))
                .collect::<Vec<_>>()
                .join("|")
        };
        let cyr = |n: usize| {
            (0..n)
                .map(|k| filter_mix_word(k, true))
                .collect::<Vec<_>>()
                .join("|")
        };
        let l = bisect(20_000, |n| accepted(&latin(n)));
        let c = bisect(20_000, |n| accepted(&cyr(n)));
        println!(
            "filter_timing: TooLarge cutoff: Latin alternation {l} words of 8 letters = {} letters (pattern {} chars); Cyrillic {c} words = {} letters (pattern {} chars); the pattern limit is {MAX_PATTERN_CHARS} chars",
            l * 8,
            latin(l).chars().count(),
            c * 8,
            cyr(c).chars().count()
        );
        let w = bisect(1_000_000, |n| accepted(&format!(r"\w{{{n}}}")));
        println!(
            "filter_timing: TooLarge cutoff: \\w{{n}} is accepted up to n = {w} (refused at {})",
            w + 1
        );
        for limit_kib in [256_usize, 1024, 2048, 4096] {
            let ok = |n: usize| {
                RegexBuilder::new(&format!(r"\w{{{n}}}"))
                    .case_insensitive(true)
                    .multi_line(true)
                    .size_limit(limit_kib * 1024)
                    .build()
                    .is_ok()
            };
            let n = bisect(1_000_000, ok);
            let t = Instant::now();
            let _ = ok(n);
            println!(
                "filter_timing: with a size limit of {limit_kib} KiB, \\w{{n}} compiles up to n = {n} ({:.1} ms for that one)",
                t.elapsed().as_secs_f64() * 1000.0
            );
        }
    }
}
