//! The tokens of message search (G1a): ONE function for the index and for the query.
//!
//! [`normalize`] cuts a text into tokens, each with its byte range in the source (the snippet
//! of a hit and the spoiler and mention ranges of a text both speak in source offsets).
//! [`words`] says what a message body puts into the index, [`query_terms`] cuts a typed
//! query. FTS5's `ascii` tokenizer only splits what we hand it on ASCII punctuation and folds
//! A-Z, so every language rule is here and an SQLite upgrade can never change a token.
//!
//! The steps, in this order (architecture §5.x, `g1-docs.md`):
//!
//! 1. The source is cut into extended grapheme clusters; each cluster is mapped on its own
//!    (a cluster is the unit that NFKC may compose, and the unit of the byte ranges): the five
//!    marks people type for the Ukrainian apostrophe become `'`, then NFKC, lower case, and
//!    the apostrophe mapping once more for what NFKC produced (fullwidth, Greek varia).
//! 2. UAX #29 word segmentation of that text (a segment with no letter or digit is dropped).
//! 3. Per word: `'` `"` the Hebrew geresh and gershayim and the invisible format characters
//!    ([`is_ignorable`]) are removed (not split on), then the word is split on any ASCII
//!    character that is not a letter or a digit and on any other punctuation that is not a mark
//!    of the character before it ([`is_separator`]). The ASCII split is what keeps a phrase
//!    out of a quoted FTS5 string: `detail=column` refuses phrases. A mark with no letter
//!    in front of it in its token is dropped ([`is_extender`]).
//! 4. Per token ([`fold_token`]): diacritics are folded (Latin, Greek: all; Cyrillic: only the
//!    grave and acute stress marks, so й and ї stay letters), Hebrew points and cantillation
//!    go, final forms fold (ם -> מ), `ß` -> `ss`, `ς` -> `σ`, ё -> е, ґ -> г.
//! 5. A token over [`MAX_TOKEN_BYTES`] is dropped (base64, keys).
//!
//! `Debug` of the types here shows counts, never a text (architecture §9.1).

use std::borrow::Cow;
use std::fmt;

use icu_normalizer::{ComposingNormalizerBorrowed, DecomposingNormalizerBorrowed};
use kourier_model::{Body, Text};
use unicode_segmentation::UnicodeSegmentation;

use super::MIN_PREFIX;

const NFKC: ComposingNormalizerBorrowed<'static> = ComposingNormalizerBorrowed::new_nfkc();
const NFC: ComposingNormalizerBorrowed<'static> = ComposingNormalizerBorrowed::new_nfc();
const NFD: DecomposingNormalizerBorrowed<'static> = DecomposingNormalizerBorrowed::new_nfd();

/// The longest token that is kept, in UTF-8 bytes. Longer ones are base64, keys, hashes.
pub(crate) const MAX_TOKEN_BYTES: usize = 64;
/// A token is folded only while its raw form is this short (a hostile text of one endless
/// word costs one pass, not a decomposition of all of it).
const MAX_RAW_TOKEN_BYTES: usize = 1024;
/// A query is cut after this many bytes of input.
pub(crate) const MAX_QUERY_BYTES: usize = 512;
/// A query keeps at most this many terms.
pub(crate) const MAX_QUERY_TERMS: usize = 32;

/// One token and where it came from.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Token {
    /// The normalised token: lower case, folded, no ASCII character but letters and digits.
    pub(crate) text: String,
    /// Start of its source, a UTF-8 byte offset into the text that was normalised (the
    /// start of the first cluster it came from).
    pub(crate) start: usize,
    /// End of its source (exclusive). Two tokens of ONE cluster (NFKC expands "℅" to
    /// "c/o") share the cluster's range.
    pub(crate) end: usize,
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Token {{ bytes: {}, source: {}..{} }}",
            self.text.len(),
            self.start,
            self.end
        )
    }
}

/// One term of a typed query.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct QueryTerm {
    /// The normalised token.
    pub(crate) text: String,
    /// Whether the term matches as a prefix (`"<text>"*`): it has at least
    /// [`MIN_PREFIX`] characters.
    pub(crate) prefix: bool,
    /// Whether the term was typed right behind the one before it, with nothing between (the
    /// source of the one before ends where this one's starts: "日本語" gives three joined terms,
    /// "日 本" two apart). The index has no positions, so a row must hold joined terms side by
    /// side in one piece of its text, which the snippet checks ([`super::snippet`]).
    pub(crate) joined: bool,
}

impl fmt::Debug for QueryTerm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "QueryTerm {{ chars: {}, prefix: {}, joined: {} }}",
            self.text.chars().count(),
            self.prefix,
            self.joined
        )
    }
}

impl QueryTerm {
    /// Whether `token` (a token of the index) matches the term as FTS5 matches it: the same
    /// bytes, or for a prefix term the token's first bytes.
    pub(crate) fn matches(&self, token: &str) -> bool {
        if self.prefix {
            token.starts_with(&self.text)
        } else {
            token == self.text
        }
    }
}

/// A character of the normalised text and the source cluster it came from.
struct Unit {
    ch: char,
    start: usize,
    end: usize,
}

/// The tokens of `source`, in order, each with its byte range in `source`.
///
/// The range of a token starts at the cluster of its first character and ends with the
/// cluster of its last, so it covers what was removed inside the word (the apostrophe of
/// "м'ята"); `normalize(&source[start..end])` gives the token again (and, for a cluster that
/// NFKC expands to several tokens, those too). Never panics, takes time and memory linear in
/// the text.
pub(crate) fn normalize(source: &str) -> Vec<Token> {
    let mut units: Vec<Unit> = Vec::with_capacity(source.len());
    let mut scratch = String::new();
    for (start, cluster) in source.grapheme_indices(true) {
        let end = start + cluster.len();
        scratch.clear();
        map_cluster(cluster, &mut scratch);
        units.extend(scratch.chars().map(|ch| Unit { ch, start, end }));
    }
    let text: String = units.iter().map(|u| u.ch).collect();
    let mut tokens = Vec::new();
    let mut next = 0;
    for segment in text.split_word_bounds() {
        let first = next;
        let mut bytes = 0;
        while bytes < segment.len() {
            bytes += units[next].ch.len_utf8();
            next += 1;
        }
        word_tokens(&units[first..next], &mut tokens);
    }
    tokens
}

/// The five marks people type for the Ukrainian apostrophe, and `'` itself. They become `'`
/// BEFORE segmentation: UAX #29 joins "м'ята" into one word and breaks at a backtick or an
/// acute accent (NFKC would even turn `´` into a space and a combining mark).
fn is_apostrophe_mark(c: char) -> bool {
    matches!(c, '\'' | '\u{2019}' | '\u{02BC}' | '`' | '\u{00B4}')
}

fn apostrophe(c: char) -> char {
    if is_apostrophe_mark(c) { '\'' } else { c }
}

/// Step 1 for one grapheme cluster: apostrophe marks, NFKC, lower case, the marks again.
fn map_cluster(cluster: &str, out: &mut String) {
    // Almost every cluster of a chat is one ASCII character.
    if let [byte @ 0..=0x7F] = cluster.as_bytes() {
        out.push(apostrophe((*byte as char).to_ascii_lowercase()));
        return;
    }
    let mapped: Cow<'_, str> = if cluster.chars().any(is_apostrophe_mark) {
        Cow::Owned(cluster.chars().map(apostrophe).collect())
    } else {
        Cow::Borrowed(cluster)
    };
    for c in NFKC.normalize(&mapped).chars().flat_map(char::to_lowercase) {
        out.push(apostrophe(c));
    }
}

/// Characters that are removed from inside a word without splitting it: the apostrophe (the
/// marks of [`is_apostrophe_mark`] are `'` by now), the ASCII double quote and the Hebrew
/// geresh and gershayim (UAX #29 keeps צה"ל and צה״ל one word), and the invisible format
/// characters that carry no letter: soft hyphen, the combining grapheme joiner, the Arabic
/// letter mark, the joiners and bidi marks and isolates (Persian words are typed with and
/// without the zero-width non-joiner), the word joiner, variation selectors and the byte order
/// mark. The zero-width SPACE (U+200B) is not among them: it separates words.
fn is_ignorable(c: char) -> bool {
    matches!(
        c,
        '\'' | '"'
            | '\u{05F3}'
            | '\u{05F4}'
            | '\u{00AD}'
            | '\u{034F}'
            | '\u{061C}'
            | '\u{180E}'
            | '\u{200C}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{206F}'
            | '\u{FE00}'..='\u{FE0F}'
            | '\u{FEFF}'
    )
}

/// The split of a word: any ASCII character that is not a letter or a digit (FTS5's `ascii`
/// tokenizer splits on exactly these, so a token that holds none is ONE token for it), and any
/// other character that is neither a letter or digit nor a mark of the character before it
/// (the middle dot or fraction slash that UAX #29 lets join two letters). A token therefore
/// starts with a letter or a digit and ends with a letter, a digit or a mark.
fn is_separator(c: char) -> bool {
    if c.is_ascii() {
        return !c.is_ascii_alphanumeric();
    }
    !c.is_alphanumeric() && !is_extender(c)
}

/// Hebrew points and cantillation marks (U+0591 to U+05C7) but not the four punctuation
/// signs inside that block: maqaf (U+05BE, a hyphen), paseq, sof pasuq and nun hafukha.
fn is_hebrew_point(c: char) -> bool {
    matches!(
        c,
        '\u{0591}'..='\u{05BD}'
            | '\u{05BF}'
            | '\u{05C1}'..='\u{05C2}'
            | '\u{05C4}'..='\u{05C5}'
            | '\u{05C7}'
    )
}

/// Whether UAX #29 glues `c` to the character before it (a combining mark, a spacing mark, a
/// joiner, a format character): the same table that cuts the words, so the two never disagree
/// (the grapheme table of the crate does, for a few scripts).
fn is_extender(c: char) -> bool {
    if c.is_ascii() {
        return false;
    }
    let mut buf = [0u8; 5];
    buf[0] = b' ';
    let len = c.encode_utf8(&mut buf[1..]).len();
    std::str::from_utf8(&buf[..=len]).is_ok_and(|s| s.split_word_bounds().count() == 1)
}

/// The removed characters that carry marks of their own: the apostrophe, the double quote, the
/// Hebrew geresh and gershayim. The other invisible characters of [`is_ignorable`] are format
/// characters that UAX #29 looks through.
fn is_mark_carrier(c: char) -> bool {
    matches!(c, '\'' | '"' | '\u{05F3}' | '\u{05F4}')
}

/// Steps 3 and 5 for one UAX #29 segment.
fn word_tokens(units: &[Unit], out: &mut Vec<Token>) {
    if !units.iter().any(|u| u.ch.is_alphanumeric()) {
        return;
    }
    let mut raw = String::new();
    let mut span: Option<(usize, usize)> = None;
    let mut too_long = false;
    // Whether the character that carries the marks now coming was removed or split on: a mark
    // goes with its carrier, never to the letter that happens to come before or after.
    let mut carrier_removed = false;
    for u in units {
        if is_ignorable(u.ch) {
            carrier_removed |= is_mark_carrier(u.ch);
            continue;
        }
        if is_separator(u.ch) {
            finish_token(&mut raw, &mut span, &mut too_long, out);
            carrier_removed = true;
            continue;
        }
        // A mark of a removed carrier, or with no letter before it in this token, belongs to
        // what stood in front, not to the token: UAX #29 glues it there, and a token that kept
        // it would not give the same token when read alone.
        if is_extender(u.ch) && (raw.is_empty() || carrier_removed) {
            continue;
        }
        carrier_removed = false;
        if raw.len() + u.ch.len_utf8() > MAX_RAW_TOKEN_BYTES {
            too_long = true;
        } else {
            raw.push(u.ch);
        }
        span = Some(span.map_or((u.start, u.end), |(start, _)| (start, u.end)));
    }
    finish_token(&mut raw, &mut span, &mut too_long, out);
}

fn finish_token(
    raw: &mut String,
    span: &mut Option<(usize, usize)>,
    too_long: &mut bool,
    out: &mut Vec<Token>,
) {
    if let Some((start, end)) = span.take()
        && !*too_long
    {
        let text = fold_token(raw);
        if text.len() <= MAX_TOKEN_BYTES && text.chars().any(char::is_alphanumeric) {
            out.push(Token { text, start, end });
        }
    }
    raw.clear();
    *too_long = false;
}

/// What a combining mark follows.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Base {
    /// Nothing yet: the mark has no letter in this token.
    Start,
    /// Latin (and a digit) or Greek.
    LatinGreek,
    Cyrillic,
    Other,
}

fn base_of(c: char) -> Base {
    match c {
        '0'..='9'
        | 'a'..='z'
        | '\u{00C0}'..='\u{024F}'
        | '\u{1E00}'..='\u{1EFF}'
        | '\u{2C60}'..='\u{2C7F}'
        | '\u{A720}'..='\u{A7FF}'
        | '\u{0370}'..='\u{03FF}'
        | '\u{1F00}'..='\u{1FFF}' => Base::LatinGreek,
        '\u{0400}'..='\u{052F}'
        | '\u{1C80}'..='\u{1C8F}'
        | '\u{2DE0}'..='\u{2DFF}'
        | '\u{A640}'..='\u{A69F}' => Base::Cyrillic,
        _ => Base::Other,
    }
}

/// The combining marks that are accents of a letter: the diacritical blocks and the marks for
/// symbols.
fn is_combining_mark(c: char) -> bool {
    matches!(
        c,
        '\u{0300}'..='\u{036F}'
            | '\u{1AB0}'..='\u{1AFF}'
            | '\u{1DC0}'..='\u{1DFF}'
            | '\u{20D0}'..='\u{20FF}'
            | '\u{FE20}'..='\u{FE2F}'
    )
}

/// Step 4: the folds of one token (lower case NFKC text with no ASCII separator).
///
/// It works on the finished token, where the marks stand next to their letters whatever was
/// removed between them, so a second pass over a token changes nothing.
fn fold_token(raw: &str) -> String {
    let decomposed = NFD.normalize(raw);
    let mut kept = String::with_capacity(decomposed.len());
    let mut base = Base::Start;
    let mut letter = '\0';
    for c in decomposed.chars() {
        if is_combining_mark(c) {
            let dropped = match base {
                // An accent on Latin or Greek goes; so does a mark with no letter.
                Base::Start | Base::LatinGreek => true,
                // Only the stress marks of Cyrillic ("за́мок") and the diaeresis of ё: the breve
                // and the diaeresis of the other letters make й and ї letters of their own and
                // are put back by NFC below. The diaeresis goes here, on the decomposed form, so
                // that no ё is ever composed again (NFC would do it after the fold of ё to е).
                Base::Cyrillic => {
                    c == '\u{0300}' || c == '\u{0301}' || (c == '\u{0308}' && letter == '\u{0435}')
                }
                Base::Other => false,
            };
            if !dropped {
                kept.push(c);
            }
            continue;
        }
        if is_hebrew_point(c) {
            continue;
        }
        // Any other mark stays, and the letter it follows stays the base of what comes after.
        if is_extender(c) {
            kept.push(c);
            continue;
        }
        base = base_of(c);
        letter = c;
        kept.push(c);
    }
    let mut out = String::with_capacity(kept.len());
    for c in NFC.normalize(&kept).chars() {
        match c {
            'ß' => out.push_str("ss"),
            'ς' => out.push('σ'),
            'ё' => out.push('е'),
            'ґ' => out.push('г'),
            // Hebrew final forms: ך ם ן ף ץ -> כ מ נ פ צ, so a prefix of a word matches
            // the word's other forms.
            '\u{05DA}' => out.push('\u{05DB}'),
            '\u{05DD}' => out.push('\u{05DE}'),
            '\u{05DF}' => out.push('\u{05E0}'),
            '\u{05E3}' => out.push('\u{05E4}'),
            '\u{05E5}' => out.push('\u{05E6}'),
            c => out.push(c),
        }
    }
    // The folds can leave a letter next to a mark it now composes with (ё with a breve is е
    // and a breve, and NFC makes that one letter): compose once more, so the form is stable.
    NFC.normalize(&out).into_owned()
}

/// Every piece of text of a message body that search reads, in the order of the message
/// (one string each, so no token ever crosses from one to the next):
///
/// - a text: its body (a mention is one U+FFFC there; the name is not indexed);
/// - a media message: the caption, each attachment's caption and each attachment's file
///   name, except the name of an attachment that is only the rest of a long text;
/// - a contact card: the name and the organization (the phone numbers, e-mail addresses
///   and the like in `identifiers` are NOT indexed);
/// - a location: the name and the address (no coordinates);
/// - a poll: the question and the options.
///
/// Nothing else is searched: a sticker, a system notice (call lines included), a
/// placeholder, an unsupported kind. The match has no `_ =>`: a lane that adds a `Body`
/// variant must choose.
pub(crate) fn searched_texts(body: &Body) -> Vec<Piece<'_>> {
    fn plain(text: &str) -> Piece<'_> {
        Piece {
            text,
            formatted: None,
            file: None,
        }
    }
    fn formatted(text: &Text) -> Piece<'_> {
        Piece {
            text: text.body.as_str(),
            formatted: Some(text),
            file: None,
        }
    }
    let mut out = Vec::new();
    match body {
        Body::Text(text) => out.push(formatted(text)),
        Body::Media {
            attachments,
            caption,
        } => {
            out.extend(caption.iter().map(formatted));
            for (index, attachment) in attachments.iter().enumerate() {
                out.extend(attachment.caption.iter().map(formatted));
                if !attachment.long_text
                    && let Some(name) = attachment.file_name.as_deref()
                {
                    out.push(Piece {
                        text: name,
                        formatted: None,
                        file: Some(u32::try_from(index).unwrap_or(u32::MAX)),
                    });
                }
            }
        }
        Body::Contact(card) => {
            out.extend(card.name.as_deref().map(plain));
            out.extend(card.organization.as_deref().map(plain));
        }
        Body::Location(location) => {
            out.extend(location.name.as_deref().map(plain));
            out.extend(location.address.as_deref().map(plain));
        }
        Body::Poll(poll) => {
            out.push(plain(poll.question.as_str()));
            out.extend(poll.options.iter().map(|o| plain(o.as_str())));
        }
        Body::Sticker(_) | Body::System(_) | Body::Placeholder(_) | Body::Unsupported { .. } => {}
    }
    out
}

/// One piece of text that search reads in a message body ([`searched_texts`]). No `Debug`:
/// it holds the text.
#[derive(Clone, Copy)]
pub(crate) struct Piece<'a> {
    /// The text itself.
    pub(crate) text: &'a str,
    /// The formatted text this piece is the body of (a message's text, a caption), whose
    /// spoiler and mention ranges speak in `text`'s offsets; `None` for a plain string (a
    /// file name, a contact's name, a place, a poll's question or option).
    pub(crate) formatted: Option<&'a Text>,
    /// For a file name: the attachment's place among the message's attachments (the
    /// attachment's `idx`).
    pub(crate) file: Option<u32>,
}

/// What a body puts into the `words` column: its tokens, one space between. `None` when the
/// body is not searched or holds no token (a text of emoji only): nothing is written then.
pub(crate) fn words(body: &Body) -> Option<String> {
    let mut out = String::new();
    for piece in searched_texts(body) {
        for token in normalize(piece.text) {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(&token.text);
        }
    }
    (!out.is_empty()).then_some(out)
}

/// The terms of a typed query: the first [`MAX_QUERY_BYTES`] bytes of it, cut as the index
/// cuts a text, at most [`MAX_QUERY_TERMS`] terms. A term holds no ASCII character but
/// letters and digits, so `"`, `*`, `:`, `(` and `NEAR` of his text are nothing or plain
/// words, and each term is exactly one token for FTS5. Empty when the query holds no word.
pub(crate) fn query_terms(input: &str) -> Vec<QueryTerm> {
    let mut end = input.len().min(MAX_QUERY_BYTES);
    while !input.is_char_boundary(end) {
        end -= 1;
    }
    let mut previous_end = None;
    normalize(&input[..end])
        .into_iter()
        .take(MAX_QUERY_TERMS)
        .map(|token| {
            let joined = previous_end == Some(token.start);
            previous_end = Some(token.end);
            QueryTerm {
                prefix: token.text.chars().count() >= MIN_PREFIX,
                joined,
                text: token.text,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests;
