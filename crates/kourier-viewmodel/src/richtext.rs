//! Message text → whitelisted Qt RichText (architecture §8.4, §9.4).
//!
//! The output uses only `<b> <i> <s> <code> <br/>` and `<a href>` with the schemes `http`,
//! `https` and `sgnl`, plus the mention scheme below; every text run is HTML-escaped, so markup in a message body is shown as
//! text and never reaches Qt as markup. Spoilers are not markup: their ranges are returned in
//! UTF-16 offsets of the *displayed* text (mentions expand to `@Name`, tabs to spaces), which
//! is what `TextEdit.positionToRectangle` takes, and QML draws opaque rectangles over them.
//!
//! Whitespace is preserved without CSS: runs of spaces become `&nbsp;` after the first and
//! newlines become `<br/>`, each counting as one displayed character like in Qt's document.
//!
//! A text makes links by itself: bare `http://` and `https://` URLs, and addresses without a scheme
//! by Signal Desktop's rule ([`find_urls`], its detector `Linkify.dom.tsx`), which open as `https://`.

use std::collections::BTreeSet;

use kourier_model::text::{MENTION_PLACEHOLDER, validate_range};
use kourier_model::{RangeKind, Text, UserId};

use crate::names::Names;

/// URL schemes a link may use. Anything else renders as plain text.
pub const ALLOWED_SCHEMES: [&str; 3] = ["http", "https", "sgnl"];

/// The scheme of the link a mention outside any link becomes: `kourier:mention/<i>`, `<i>` the
/// index into [`RichText::mentions`] (people pages P3: a click opens that person). It is made here
/// and nowhere else: [`allowed_href`] refuses `kourier:`, so a link range of a message that names
/// it stays plain text (a contact cannot make a link that opens someone's page).
pub const MENTION_HREF: &str = "kourier:mention/";

/// A range of displayed text in UTF-16 code units.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Span16 {
    /// Start offset.
    pub start: u32,
    /// Length.
    pub len: u32,
}

/// A message text rendered for a read-only `TextEdit { textFormat: TextEdit.RichText }`.
#[derive(Clone, Default, PartialEq, Eq, Hash)]
pub struct RichText {
    /// Whitelisted HTML.
    pub html: String,
    /// The displayed text without markup (accessibility, copy, tests).
    pub plain: String,
    /// Spoiler ranges in UTF-16 offsets of [`Self::plain`], sorted, non-overlapping.
    pub spoilers: Vec<Span16>,
    /// The text is one to three emoji only: show it large.
    pub jumbo: bool,
    /// The text goes on beyond what [`Self::html`] shows ("Read more", [`crate::readmore`]).
    pub more: bool,
    /// How many words of the text are not shown, when [`Self::more`] (the hint in "Read
    /// more"): the pieces of the hidden part between spaces and line breaks. 0 otherwise.
    pub more_words: u32,
    /// The whole displayed text, when [`Self::more`] (copy, quote, edit); empty otherwise, then
    /// [`Self::plain`] is the whole text.
    pub whole: String,
    /// The addresses of the links that are shown, each once, in reading order ("Copy Link
    /// Address"). A link a spoiler covers, even in part, is not listed: copying must not show it.
    pub links: Vec<String>,
    /// The people the mentions of the text name, in the order of their ranges: the `<i>` of a
    /// `kourier:mention/<i>` link ([`MENTION_HREF`]) indexes this. Every valid mention range has
    /// an entry; only one that stands outside a link has the link.
    pub mentions: Vec<UserId>,
    /// Per entry of [`Self::mentions`]: the mention lies under a spoiler (it does not open while
    /// the spoiler hides it).
    pub mention_spoilers: Vec<bool>,
}

crate::safe_debug!(RichText {
    spoilers,
    jumbo,
    more
});

impl RichText {
    /// The whole displayed text, also when only a part of it is shown.
    pub fn full_plain(&self) -> &str {
        if self.more { &self.whole } else { &self.plain }
    }
}

/// Whether `c` draws nothing or reorders the text around it, whatever stands beside it, so that
/// an address holding it can read as another address than the one it opens (an override such as
/// U+202E turns the rest of a path around; a zero-width character splits a host into two that
/// look alike; a tag character or a soft hyphen hides inside a path or a query). The
/// context-free set, exactly:
///
/// - U+00AD (soft hyphen), U+034F (combining grapheme joiner), U+061C (Arabic letter mark);
/// - U+115F and U+1160 (Hangul choseong and jungseong fillers), U+3164 and U+FFA0 (Hangul
///   fillers, full and half width): letters that draw as blank;
/// - U+17B4 and U+17B5 (Khmer inherent vowels), U+180E (Mongolian vowel separator);
/// - U+200B (zero-width space), U+200E and U+200F (left-to-right and right-to-left mark);
/// - U+202A..=U+202E (embeddings, pop, overrides);
/// - U+2060..=U+206F: the word joiner, the invisible function application, times, separator
///   and plus (U+2061..=U+2064), the unassigned U+2065, the isolates and their pop
///   (U+2066..=U+2069) and the deprecated format characters (U+206A..=U+206F, which also steer
///   bidi and shaping);
/// - U+FEFF (zero-width no-break space, the byte order mark);
/// - U+FFF0..=U+FFFB: the unassigned U+FFF0..=U+FFF8, then the interlinear annotation anchor,
///   separator and terminator;
/// - U+1BCA0..=U+1BCA3 (shorthand format controls), U+1D173..=U+1D17A (musical symbol beams
///   and phrase marks);
/// - U+E0000..=U+E00FF: the tag characters (U+E0000..=U+E007F, which draw nothing and can spell
///   a hidden word) and the unassigned U+E0080..=U+E00FF; and U+E01F0..=U+E0FFF, the unassigned
///   rest of the block after the variation selectors.
///
/// The unassigned code points of those blocks are in because they are default-ignorable: the
/// shaper hides every one of them, assigned or not, and Qt 6.11.1 draws `a` U+FFF0 `b` as wide
/// as `ab` (measured), while an ordinary unassigned code point draws a box.
///
/// NOT in it, on purpose: the zero-width non-joiner U+200C and joiner U+200D (they change how
/// their neighbours are shaped, so they are judged by their place, see [`hides_in_link`]); the
/// variation selectors U+FE00..=U+FE0F and U+E0100..=U+E01EF (an emoji domain carries U+FE0F,
/// and a selector changes a visible glyph; after an ASCII character one draws nothing, a known
/// limit: it stays in a path or a query, and in a host Qt's `QUrl` drops it); the Mongolian free
/// variation selectors
/// U+180B..=U+180D and U+180F (the same: they change a visible glyph like the other variation
/// selectors). U+180E stays in: it draws nothing at all.
///
/// All of them are category Cf, Mn, Lo or unassigned, which `char::is_control` (Cc only) does
/// not name, and none is white space (U+2028 and U+2029, which are, end a URL as white space
/// already). It is public so that other code which judges text for the same reason keeps to one
/// rule: the cleaner of file names (F3) reuses this set and [`hides_in_link`].
pub fn is_invisible_or_bidi(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}'
            | '\u{034F}'
            | '\u{061C}'
            | '\u{115F}'..='\u{1160}'
            | '\u{17B4}'..='\u{17B5}'
            | '\u{180E}'
            | '\u{200B}'
            | '\u{200E}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{206F}'
            | '\u{3164}'
            | '\u{FEFF}'
            | '\u{FFA0}'
            | '\u{FFF0}'..='\u{FFFB}'
            | '\u{1BCA0}'..='\u{1BCA3}'
            | '\u{1D173}'..='\u{1D17A}'
            | '\u{E0000}'..='\u{E00FF}'
            | '\u{E01F0}'..='\u{E0FFF}'
    )
}

/// The code points with Canonical_Combining_Class 9 (Virama: the sign that kills a consonant's
/// vowel, Unicode 17 as Qt 6.11.1 has it), sorted. RFC 5892 A.1 and A.2 allow a zero-width
/// non-joiner or joiner after one.
const VIRAMAS: [u32; 69] = [
    0x094D, 0x09CD, 0x0A4D, 0x0ACD, 0x0B4D, 0x0BCD, 0x0C4D, 0x0CCD, 0x0D3B, 0x0D3C, 0x0D4D, 0x0DCA,
    0x0E3A, 0x0EBA, 0x0F84, 0x1039, 0x103A, 0x1714, 0x1715, 0x1734, 0x17D2, 0x1A60, 0x1B44, 0x1BAA,
    0x1BAB, 0x1BF2, 0x1BF3, 0x2D7F, 0xA806, 0xA82C, 0xA8C4, 0xA953, 0xA9C0, 0xAAF6, 0xABED,
    0x10A3F, 0x11046, 0x11070, 0x1107F, 0x110B9, 0x11133, 0x11134, 0x111C0, 0x11235, 0x112EA,
    0x1134D, 0x113CE, 0x113CF, 0x113D0, 0x11442, 0x114C2, 0x115BF, 0x1163F, 0x116B6, 0x1172B,
    0x11839, 0x1193D, 0x1193E, 0x119E0, 0x11A34, 0x11A47, 0x11A99, 0x11C3F, 0x11D44, 0x11D45,
    0x11D97, 0x11F41, 0x11F42, 0x1612F,
];

/// Whether `c` is a virama (Canonical_Combining_Class 9).
fn is_virama(c: char) -> bool {
    VIRAMAS.binary_search(&u32::from(c)).is_ok()
}

/// The zero-width non-joiner U+200C and the zero-width joiner U+200D.
fn is_joiner(c: char) -> bool {
    matches!(c, '\u{200C}' | '\u{200D}')
}

/// Whether a joiner between `prev` and `next` is part of the spelling: straight after a virama
/// ([`is_virama`]), whatever follows it or nothing (RFC 5892 A.1 and A.2, rule 1: legacy
/// Malayalam chillu, consonant U+0D4D U+200D, and old Bengali khanda ta, U+09A4 U+09CD U+200D,
/// end a word so; the joiner is what makes the sign visible); or both neighbours exist, are
/// not ASCII, and are neither white space, a control character, a joiner nor in the
/// context-free set ([`is_invisible_or_bidi`]). Between two letters that join (Persian and other
/// Arabic-script words) or inside an emoji sequence the joiner changes how the neighbours are
/// drawn; between ASCII letters (`exam` U+200D `ple`) it draws nothing and only hides a second
/// address behind a first.
fn joiner_in_context(prev: Option<char>, next: Option<char>) -> bool {
    if prev.is_some_and(is_virama) {
        return true;
    }
    let neighbour = |n: Option<char>| {
        n.is_some_and(|n| {
            !n.is_ascii()
                && !n.is_whitespace()
                && !n.is_control()
                && !is_joiner(n)
                && !is_invisible_or_bidi(n)
        })
    };
    neighbour(prev) && neighbour(next)
}

/// Whether `c`, standing between `prev` and `next` in an address (`None` at its start or end),
/// must not be part of a link: a character of the context-free set ([`is_invisible_or_bidi`]), or
/// a zero-width non-joiner or joiner (U+200C, U+200D) out of context. A joiner is KEPT only
/// straight after a virama, whatever follows ([`joiner_in_context`]), or when both neighbours
/// exist and are not ASCII, white space, control characters, joiners or members of the set
/// (Persian `می` U+200C `خواهم`, an Indic consonant, U+094D, U+200D and a consonant, an emoji ZWJ
/// sequence); at the start or the end of an address, beside an ASCII character or beside
/// another joiner it hides, like the set. [`find_urls`] ends a URL there
/// and [`allowed_href`] refuses an address that holds one. It is public so that other code which
/// judges text for the same reason keeps to one rule: the cleaner of file names (F3) reuses it
/// together with [`is_invisible_or_bidi`].
pub fn hides_in_link(prev: Option<char>, c: char, next: Option<char>) -> bool {
    is_invisible_or_bidi(c) || (is_joiner(c) && !joiner_in_context(prev, next))
}

/// The longest address that may become a link and the longest host in it, in UTF-16 units: Signal
/// Desktop's `isLinkSneaky` (a very long address can hide something, and it costs time). A security
/// bound, architecture §13 phase H.
const MAX_HREF_UNITS: usize = 4096;
const MAX_HOST_UNITS: usize = 2048;

/// Whether `url` may become a link: an allowed scheme, at most [`MAX_HREF_UNITS`], no whitespace or
/// control characters, no character that hides in an address ([`hides_in_link`]: the invisible and
/// bidi set, and a joiner out of context), no box-drawing or block character (U+2500..=U+25FF:
/// `U+2571` reads as a slash; Desktop's `isLinkSneaky`), and `//` plus a sound host for http(s)
/// ([`sound_authority`]). Returns the URL to put in `href` (unescaped).
pub fn allowed_href(url: &str) -> Option<&str> {
    if url.is_empty() || url.encode_utf16().count() > MAX_HREF_UNITS {
        return None;
    }
    let mut prev = None;
    let mut chars = url.chars().peekable();
    while let Some(c) = chars.next() {
        if c.is_whitespace()
            || c.is_control()
            || ('\u{2500}'..='\u{25FF}').contains(&c)
            || hides_in_link(prev, c, chars.peek().copied())
        {
            return None;
        }
        prev = Some(c);
    }
    let (scheme, rest) = url.split_once(':')?;
    if scheme.is_empty()
        || !scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    {
        return None;
    }
    let scheme = scheme.to_ascii_lowercase();
    if !ALLOWED_SCHEMES.contains(&scheme.as_str()) {
        return None;
    }
    if scheme.starts_with("http") {
        if !sound_authority(rest.strip_prefix("//")?) {
            return None;
        }
    } else if rest.is_empty() {
        return None;
    }
    Some(url)
}

/// What Signal Desktop's `isLinkSneaky` asks of the part of an http(s) address after `//`, up to the
/// first `/`, `?` or `#`: no user name or password (no `@`: `https://bank.example@evil.example` opens
/// `evil.example`; a backslash ends nothing here, so `bank.example\@evil.example` is refused whatever
/// the opener takes it for), a host that is not empty, holds no `%` (a host has no encoded
/// characters), has at most [`MAX_HOST_UNITS`] and no empty label. NOT taken from it: two labels at
/// least (`http://localhost:3000` and an intranet's name are real addresses that the "Open Link?"
/// dialog shows as they are, and no other client enforces it: Desktop's own choice); no ASCII beside
/// other characters in a host (`https://münchen.de` and `https://میخواهم.ir` are real, and written
/// with their scheme they are meant: that rule holds only for a match without a scheme,
/// [`bare_domain_at`]); no parse by a URL parser (there is none here; what `QUrl` refuses does not
/// open).
fn sound_authority(after_slashes: &str) -> bool {
    let authority = after_slashes
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default();
    if authority.contains('@') {
        return false;
    }
    let host = if authority.starts_with('[') {
        // An IPv6 literal holds colons; its port comes after the bracket.
        authority.split_inclusive(']').next().unwrap_or_default()
    } else {
        authority
            .rsplit_once(':')
            .map_or(authority, |(host, _)| host)
    };
    !host.is_empty()
        && !host.contains('%')
        && host.encode_utf16().count() <= MAX_HOST_UNITS
        && host.split('.').all(|label| !label.is_empty())
}

/// Byte ranges of the links a text holds by itself, in order: bare `http://` and `https://` URLs
/// and addresses without a scheme ([`bare_domain_at`]), as Signal Desktop linkifies them. A URL
/// starts at a word boundary and ends before whitespace, `<>"`, a control, a character that hides
/// in an address ([`hides_in_link`]: the invisible and bidi set, and a zero-width joiner or
/// non-joiner out of context) or a mention character; trailing punctuation, trailing joiners and
/// unbalanced closing brackets are not part of it. The character that ends it stays in the text,
/// outside the link. What [`allowed_href`] refuses is not returned: its text stays plain, and
/// nothing inside it is looked at again.
pub fn find_urls(body: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut names = EmailNames::default();
    let mut from = 0;
    for (start, end) in explicit_urls(body) {
        find_bare_domains(body, from, start, &mut names, &mut out);
        if allowed_href(&body[start..end]).is_some() {
            out.push((start, end));
        }
        from = end;
    }
    find_bare_domains(body, from, body.len(), &mut names, &mut out);
    out
}

/// What a match of [`find_urls`] opens: itself when it has its scheme, else `https://` and the
/// match. (Desktop opens a match without a scheme as `http://`; here the address that opens is
/// the encrypted one, and the "Open Link?" dialog shows it whole. A match without a scheme cannot
/// start with one: `http` alone is no host.)
pub(crate) fn href_of_match(found: &str) -> String {
    let has_scheme = |scheme: &str| {
        found
            .get(..scheme.len())
            .is_some_and(|start| start.eq_ignore_ascii_case(scheme))
    };
    if has_scheme("http://") || has_scheme("https://") {
        found.to_owned()
    } else {
        format!("https://{found}")
    }
}

/// Byte ranges of the bare URLs of `body` that have their scheme, before [`allowed_href`] is asked.
fn explicit_urls(body: &str) -> Vec<(usize, usize)> {
    let lower = body.to_ascii_lowercase();
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(pos) = lower[from..].find("http").map(|p| p + from) {
        let rest = &lower[pos..];
        let scheme_len = if rest.starts_with("https://") {
            8
        } else if rest.starts_with("http://") {
            7
        } else {
            from = pos + 4;
            continue;
        };
        let boundary = body[..pos]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_alphanumeric() && c != '_' && c != '/');
        let end = url_tail_end(body, pos + scheme_len);
        let end = trim_url_end(&body[pos..end]) + pos;
        if boundary && end > pos + scheme_len {
            out.push((pos, end));
            from = end;
        } else {
            from = pos + scheme_len;
        }
    }
    out
}

/// Where the part of an address that starts at `from` ends: before white space, `<>"`, a control,
/// a character that hides in an address or a mention character.
fn url_tail_end(text: &str, from: usize) -> usize {
    let mut end = from;
    let mut prev = text[..from].chars().next_back();
    let mut chars = text[from..].char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if c.is_whitespace()
            || c.is_control()
            || hides_in_link(prev, c, chars.peek().map(|&(_, next)| next))
            || matches!(c, '<' | '>' | '"' | MENTION_PLACEHOLDER)
        {
            break;
        }
        end = from + i + c.len_utf8();
        prev = Some(c);
    }
    end
}

/// Length of `url` without trailing punctuation, unbalanced closing brackets and joiners. (A
/// joiner is kept inside a URL only between two characters of the URL, [`hides_in_link`]; one that
/// the scan took because a character stood after it, which then ended the URL or was trimmed, is
/// at the end now and goes too, so that every URL found is one `allowed_href` takes. One straight
/// after a virama stays: it needs no character after it, [`joiner_in_context`].)
fn trim_url_end(url: &str) -> usize {
    let mut end = url.len();
    // The brackets still inside `url[..end]`, counted once and kept as the loop strips closing ones
    // (parentheses, square and curly brackets): a run of `)` after an address was quadratic when the
    // whole text was counted again for every one stripped, and a hostile description is such a text.
    let (mut open, mut close) = ([0usize; 3], [0usize; 3]);
    for c in url.chars() {
        match c {
            '(' => open[0] += 1,
            ')' => close[0] += 1,
            '[' => open[1] += 1,
            ']' => close[1] += 1,
            '{' => open[2] += 1,
            '}' => close[2] += 1,
            _ => {}
        }
    }
    loop {
        let Some(c) = url[..end].chars().next_back() else {
            return end;
        };
        let strip = match c {
            '.' | ',' | ':' | ';' | '!' | '?' | '\'' | '*' | '»' | '”' | '’' => true,
            c if is_joiner(c) => !url[..end - c.len_utf8()]
                .chars()
                .next_back()
                .is_some_and(is_virama),
            ')' | ']' | '}' => {
                let kind = match c {
                    ')' => 0,
                    ']' => 1,
                    _ => 2,
                };
                // An unbalanced closing bracket is not part of the address.
                let unbalanced = close[kind] > open[kind];
                if unbalanced {
                    close[kind] -= 1;
                }
                unbalanced
            }
            _ => false,
        };
        if !strip {
            return end;
        }
        end -= c.len_utf8();
    }
}

/// The last labels an address without a scheme may have: Signal Desktop's list
/// (`ts/components/conversation/Linkify.dom.tsx`, AGPL-3.0 like Kourier: every TLD of IANA's root
/// zone in 2010, without the reserved test domains), sorted by bytes. It applies to these matches
/// only: an address with its scheme may have any TLD. FALSE POSITIVES DESKTOP HAS TOO ARE ACCEPTED:
/// `.rs` and `.md` are ccTLDs of 2010, so `main.rs` and `readme.md` become links (and `10.am`); the
/// reader sees the whole address before anything opens.
#[rustfmt::skip]
const TLDS: [&str; 290] = [
    "ac", "ad", "ae", "aero", "af", "ag", "ai", "al", "am", "an", "ao", "aq", "ar", "arpa", "as",
    "asia", "at", "au", "aw", "ax", "az", "ba", "bb", "bd", "be", "bf", "bg", "bh", "bi", "biz",
    "bj", "bl", "bm", "bn", "bo", "bq", "br", "bs", "bt", "bv", "bw", "by", "bz", "ca", "cat", "cc",
    "cd", "cf", "cg", "ch", "ci", "ck", "cl", "cm", "cn", "co", "com", "coop", "cr", "cu", "cv",
    "cw", "cx", "cy", "cz", "de", "dj", "dk", "dm", "do", "dz", "ec", "edu", "ee", "eg", "er", "es",
    "et", "eu", "fi", "fj", "fk", "fm", "fo", "fr", "ga", "gb", "gd", "ge", "gf", "gg", "gh", "gi",
    "gl", "gm", "gn", "gov", "gp", "gq", "gr", "gs", "gt", "gu", "gw", "gy", "hk", "hm", "hn", "hr",
    "ht", "hu", "id", "ie", "il", "im", "in", "info", "int", "io", "iq", "ir", "is", "it", "je",
    "jm", "jo", "jobs", "jp", "ke", "kg", "kh", "ki", "km", "kn", "kp", "kr", "kw", "ky", "kz",
    "la", "lb", "lc", "li", "lk", "lr", "ls", "lt", "lu", "lv", "ly", "ma", "mc", "md", "me", "mf",
    "mg", "mh", "mil", "mk", "ml", "mm", "mn", "mo", "mobi", "mp", "mq", "mr", "ms", "mt", "mu",
    "museum", "mv", "mw", "mx", "my", "mz", "na", "name", "nc", "ne", "net", "nf", "ng", "ni", "nl",
    "no", "np", "nr", "nu", "nz", "om", "org", "pa", "pe", "pf", "pg", "ph", "pk", "pl", "pm", "pn",
    "pr", "pro", "ps", "pt", "pw", "py", "qa", "re", "ro", "rs", "ru", "rw", "sa", "sb", "sc", "sd",
    "se", "sg", "sh", "si", "sj", "sk", "sl", "sm", "sn", "so", "sr", "st", "su", "sv", "sx", "sy",
    "sz", "tc", "td", "tel", "tf", "tg", "th", "tj", "tk", "tl", "tm", "tn", "to", "tp", "tr",
    "travel", "tt", "tv", "tw", "tz", "ua", "ug", "uk", "um", "us", "uy", "uz", "va", "vc", "ve",
    "vg", "vi", "vn", "vu", "wf", "ws", "ye", "yt", "za", "zm", "zw", "рф", "الاردن", "السعودية",
    "امارات", "تونس", "فلسطين", "قطر", "مصر", "இலங்கை", "ලංකා", "ไทย", "中国", "中國", "台湾", "台灣", "香港",
];

/// Unicode's general category P (punctuation) without ASCII, Unicode 15, as the first and the last
/// code point of each range, sorted: what ends a word in a text (`«`, `—`, `。`, `،`, `؟`) and so
/// stands around an address without being part of it.
const PUNCTUATION: [u32; 364] = [
    0x00A1, 0x00A1, 0x00A7, 0x00A7, 0x00AB, 0x00AB, 0x00B6, 0x00B7, 0x00BB, 0x00BB, 0x00BF, 0x00BF,
    0x037E, 0x037E, 0x0387, 0x0387, 0x055A, 0x055F, 0x0589, 0x058A, 0x05BE, 0x05BE, 0x05C0, 0x05C0,
    0x05C3, 0x05C3, 0x05C6, 0x05C6, 0x05F3, 0x05F4, 0x0609, 0x060A, 0x060C, 0x060D, 0x061B, 0x061B,
    0x061D, 0x061F, 0x066A, 0x066D, 0x06D4, 0x06D4, 0x0700, 0x070D, 0x07F7, 0x07F9, 0x0830, 0x083E,
    0x085E, 0x085E, 0x0964, 0x0965, 0x0970, 0x0970, 0x09FD, 0x09FD, 0x0A76, 0x0A76, 0x0AF0, 0x0AF0,
    0x0C77, 0x0C77, 0x0C84, 0x0C84, 0x0DF4, 0x0DF4, 0x0E4F, 0x0E4F, 0x0E5A, 0x0E5B, 0x0F04, 0x0F12,
    0x0F14, 0x0F14, 0x0F3A, 0x0F3D, 0x0F85, 0x0F85, 0x0FD0, 0x0FD4, 0x0FD9, 0x0FDA, 0x104A, 0x104F,
    0x10FB, 0x10FB, 0x1360, 0x1368, 0x1400, 0x1400, 0x166E, 0x166E, 0x169B, 0x169C, 0x16EB, 0x16ED,
    0x1735, 0x1736, 0x17D4, 0x17D6, 0x17D8, 0x17DA, 0x1800, 0x180A, 0x1944, 0x1945, 0x1A1E, 0x1A1F,
    0x1AA0, 0x1AA6, 0x1AA8, 0x1AAD, 0x1B5A, 0x1B60, 0x1B7D, 0x1B7E, 0x1BFC, 0x1BFF, 0x1C3B, 0x1C3F,
    0x1C7E, 0x1C7F, 0x1CC0, 0x1CC7, 0x1CD3, 0x1CD3, 0x2010, 0x2027, 0x2030, 0x2043, 0x2045, 0x2051,
    0x2053, 0x205E, 0x207D, 0x207E, 0x208D, 0x208E, 0x2308, 0x230B, 0x2329, 0x232A, 0x2768, 0x2775,
    0x27C5, 0x27C6, 0x27E6, 0x27EF, 0x2983, 0x2998, 0x29D8, 0x29DB, 0x29FC, 0x29FD, 0x2CF9, 0x2CFC,
    0x2CFE, 0x2CFF, 0x2D70, 0x2D70, 0x2E00, 0x2E2E, 0x2E30, 0x2E4F, 0x2E52, 0x2E5D, 0x3001, 0x3003,
    0x3008, 0x3011, 0x3014, 0x301F, 0x3030, 0x3030, 0x303D, 0x303D, 0x30A0, 0x30A0, 0x30FB, 0x30FB,
    0xA4FE, 0xA4FF, 0xA60D, 0xA60F, 0xA673, 0xA673, 0xA67E, 0xA67E, 0xA6F2, 0xA6F7, 0xA874, 0xA877,
    0xA8CE, 0xA8CF, 0xA8F8, 0xA8FA, 0xA8FC, 0xA8FC, 0xA92E, 0xA92F, 0xA95F, 0xA95F, 0xA9C1, 0xA9CD,
    0xA9DE, 0xA9DF, 0xAA5C, 0xAA5F, 0xAADE, 0xAADF, 0xAAF0, 0xAAF1, 0xABEB, 0xABEB, 0xFD3E, 0xFD3F,
    0xFE10, 0xFE19, 0xFE30, 0xFE52, 0xFE54, 0xFE61, 0xFE63, 0xFE63, 0xFE68, 0xFE68, 0xFE6A, 0xFE6B,
    0xFF01, 0xFF03, 0xFF05, 0xFF0A, 0xFF0C, 0xFF0F, 0xFF1A, 0xFF1B, 0xFF1F, 0xFF20, 0xFF3B, 0xFF3D,
    0xFF3F, 0xFF3F, 0xFF5B, 0xFF5B, 0xFF5D, 0xFF5D, 0xFF5F, 0xFF65, 0x10100, 0x10102, 0x1039F,
    0x1039F, 0x103D0, 0x103D0, 0x1056F, 0x1056F, 0x10857, 0x10857, 0x1091F, 0x1091F, 0x1093F,
    0x1093F, 0x10A50, 0x10A58, 0x10A7F, 0x10A7F, 0x10AF0, 0x10AF6, 0x10B39, 0x10B3F, 0x10B99,
    0x10B9C, 0x10EAD, 0x10EAD, 0x10F55, 0x10F59, 0x10F86, 0x10F89, 0x11047, 0x1104D, 0x110BB,
    0x110BC, 0x110BE, 0x110C1, 0x11140, 0x11143, 0x11174, 0x11175, 0x111C5, 0x111C8, 0x111CD,
    0x111CD, 0x111DB, 0x111DB, 0x111DD, 0x111DF, 0x11238, 0x1123D, 0x112A9, 0x112A9, 0x1144B,
    0x1144F, 0x1145A, 0x1145B, 0x1145D, 0x1145D, 0x114C6, 0x114C6, 0x115C1, 0x115D7, 0x11641,
    0x11643, 0x11660, 0x1166C, 0x116B9, 0x116B9, 0x1173C, 0x1173E, 0x1183B, 0x1183B, 0x11944,
    0x11946, 0x119E2, 0x119E2, 0x11A3F, 0x11A46, 0x11A9A, 0x11A9C, 0x11A9E, 0x11AA2, 0x11B00,
    0x11B09, 0x11C41, 0x11C45, 0x11C70, 0x11C71, 0x11EF7, 0x11EF8, 0x11F43, 0x11F4F, 0x11FFF,
    0x11FFF, 0x12470, 0x12474, 0x12FF1, 0x12FF2, 0x16A6E, 0x16A6F, 0x16AF5, 0x16AF5, 0x16B37,
    0x16B3B, 0x16B44, 0x16B44, 0x16E97, 0x16E9A, 0x16FE2, 0x16FE2, 0x1BC9F, 0x1BC9F, 0x1DA87,
    0x1DA8B, 0x1E95E, 0x1E95F,
];

/// Whether `c` (not ASCII) is in [`PUNCTUATION`].
fn is_punctuation(c: char) -> bool {
    let c = u32::from(c);
    let at = PUNCTUATION.partition_point(|&bound| bound < c);
    at % 2 == 1 || PUNCTUATION.get(at) == Some(&c)
}

/// Whether a character that is not ASCII ends a word: white space, a control, punctuation, the
/// full-width bar, or a part of an emoji (Desktop looks for links only between emoji, so an emoji
/// beside an address is the end of the text for it). Every other character can be a letter of a
/// host: there is no table of letters to ask, the marks of Indic and Thai words are letters here
/// and `char::is_alphanumeric` does not name them all. It is linkify-it's definition, "everything
/// without punctuation, spaces and controls".
fn ends_word(c: char) -> bool {
    c.is_whitespace()
        || c.is_control()
        || is_punctuation(c)
        || c == '\u{FF5C}'
        || is_emoji_base(c)
        || is_emoji_modifier(c)
}

/// What a reader does not see: the character says nothing about where a word stands. The text
/// around an address is judged past these, so `exam` U+200B `ple.org` is the inside of a word and
/// `a@` U+2066 `example.org` is still an e-mail address, while U+200F `example.org` U+200F, as
/// right-to-left text often has it, is a link. A variation selector (U+FE00 to U+FE0F) draws
/// nothing after a letter, so `exam` U+FE0F `ple.org` is one word to the reader; after an emoji
/// base it stays with the emoji, which does end a word.
fn is_unseen(c: char) -> bool {
    is_invisible_or_bidi(c) || is_joiner(c) || matches!(c, '\u{FE00}'..='\u{FE0F}')
}

/// Whether `c`, between `prev` and `next`, is a letter of a host label. ASCII: letters and digits
/// (linkify-it also takes `$+=^|~` and the backtick; no host holds them, here they stand beside a
/// word). Otherwise what does not end a word ([`ends_word`]), is no mention character and does not
/// hide in an address ([`hides_in_link`]; so a joiner in context stays in a label).
fn is_label_char(prev: Option<char>, c: char, next: Option<char>) -> bool {
    if c.is_ascii() {
        c.is_ascii_alphanumeric()
    } else {
        !ends_word(c) && c != MENTION_PLACEHOLDER && !hides_in_link(prev, c, next)
    }
}

/// Where the host label that starts at `start` ends: letters ([`is_label_char`]) and hyphens.
fn label_end(text: &str, start: usize) -> usize {
    let mut prev = text[..start].chars().next_back();
    let mut chars = text[start..].char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        let next = chars.peek().map(|&(_, next)| next);
        if c != '-' && !is_label_char(prev, c, next) {
            return start + i;
        }
        prev = Some(c);
    }
    text.len()
}

/// Whether `label` is one a host may have: 1 to 63 characters, no hyphen at either end.
fn valid_label(label: &str) -> bool {
    !label.is_empty()
        && !label.starts_with('-')
        && !label.ends_with('-')
        && label.chars().count() <= 63
}

/// Whether `label` is a last label of an address without a scheme: in [`TLDS`], in any case, or
/// the ASCII form of an international one (`xn--` and 1 to 59 of `a-z0-9-`, which linkify-it adds
/// to any list it is given).
fn is_tld(label: &str) -> bool {
    let lower: String = label.chars().flat_map(char::to_lowercase).collect();
    if let Some(rest) = lower.strip_prefix("xn--") {
        return (1..=59).contains(&rest.len())
            && rest
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    }
    TLDS.binary_search(&lower.as_str()).is_ok()
}

/// The end of the host at `start`: at least two labels with single dots between them, the last one
/// a TLD ([`is_tld`]). (Digits are letters here, so an IP address has no TLD and is no host: Desktop
/// leaves linkify-it's `fuzzyIP` off.) A dot goes on when a letter follows it; so only the whole run
/// of labels can be the host, as with linkify-it, whose terminator refuses a dot before a letter.
fn host_at(text: &str, start: usize) -> Option<usize> {
    let mut labels = 0;
    let mut at = start;
    loop {
        let end = label_end(text, at);
        let label = &text[at..end];
        if !valid_label(label) {
            return None;
        }
        labels += 1;
        let after_dot = text[end..].strip_prefix('.');
        let mut after = after_dot.unwrap_or("").chars();
        let starts_label = after
            .next()
            .is_some_and(|c| is_label_char(Some('.'), c, after.next()));
        if !starts_label {
            return (labels >= 2 && is_tld(label)).then_some(end);
        }
        at = end + 1;
    }
}

/// Where a port after `host_end` ends, or `host_end` when none stands there: `:` and a number up
/// to 65535 of at most five digits, a five-digit one without a leading zero (linkify-it's
/// `src_port`). What is left of a longer number then fails [`may_end_host`], which refuses `:` and
/// a digit.
fn port_end(text: &str, host_end: usize) -> usize {
    let Some(rest) = text[host_end..].strip_prefix(':') else {
        return host_end;
    };
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    let fits = match digits {
        1..=4 => true,
        5 => !rest.starts_with('0') && rest[..5].parse::<u32>().is_ok_and(|n| n <= 65535),
        _ => false,
    };
    if fits {
        host_end + 1 + digits
    } else {
        host_end
    }
}

/// Whether a host with its port may end where `rest` starts: linkify-it's `src_host_terminator`,
/// judged on what the reader sees ([`is_unseen`]). The end of the text or the end of a word, but not
/// a hyphen, an underscore, `:` and a digit, `.-`, or a dot before another word.
fn may_end_host(rest: &str) -> bool {
    let mut seen = rest.chars().filter(|c| !is_unseen(*c));
    let ends_word_here = |c: char| {
        if c.is_ascii() {
            !c.is_ascii_alphanumeric() && !matches!(c, '$' | '+' | '=' | '^' | '`' | '|' | '~')
        } else {
            ends_word(c)
        }
    };
    let Some(first) = seen.next() else {
        return true;
    };
    match (first, seen.next()) {
        ('-' | '_', _) => false,
        (':', Some(next)) => !next.is_ascii_digit(),
        ('.', Some(next)) => next != '-' && ends_word_here(next),
        (c, _) => ends_word_here(c),
    }
}

/// Whether a host may start after `before`, linkify-it's rule for what stands in front of a match:
/// the start of the text, or (past what is not seen, [`is_unseen`]) a character that ends a word and
/// is none of `.:/-_@`. So the domain of `a@example.org` and `ftp://example.org` are no links, nor is
/// any later label of a run of labels.
fn may_start_host(before: &str) -> bool {
    let Some(c) = before.chars().rev().find(|c| !is_unseen(*c)) else {
        return true;
    };
    if c.is_ascii() {
        !c.is_ascii_alphanumeric() && !matches!(c, '.' | ':' | '/' | '-' | '_' | '@')
    } else {
        ends_word(c)
    }
}

/// What one look for the name of an e-mail address found: it goes on to `until`, and holds an
/// address or not. A second match inside the same run has the same answer, so a text of `a.de,`
/// repeated is read once and not once per match (a contact's text costs no more than its length).
#[derive(Default)]
struct EmailNames {
    until: usize,
    email: bool,
}

impl EmailNames {
    /// Whether what ends at `end` is part of the name of an e-mail address: the characters a name
    /// may hold (linkify-it's `src_email_name`: `a-zA-Z0-9-;:&=+$,."_`) lead from `end` to `@` and
    /// a host. Linkify-it's own e-mail rule also looks at what stands before the name, and leaves
    /// `john.de` of `[john.de@example.org]` a link; here it stays text.
    fn holds(&mut self, text: &str, end: usize) -> bool {
        if end <= self.until {
            return self.email;
        }
        let mut seen = text[end..]
            .char_indices()
            .map(|(i, c)| (end + i, c))
            .filter(|(_, c)| !is_unseen(*c));
        let stop = seen.find(|&(_, c)| {
            !c.is_ascii_alphanumeric()
                && !matches!(
                    c,
                    '-' | ';' | ':' | '&' | '=' | '+' | '$' | ',' | '.' | '"' | '_'
                )
        });
        self.until = stop.map_or(text.len(), |(i, _)| i);
        self.email = matches!(stop, Some((_, '@')))
            && seen.next().is_some_and(|(host, _)| {
                host_at(text, host).is_some_and(|end| may_end_host(&text[port_end(text, end)..]))
            });
        self.email
    }
}

/// The end of the address without a scheme that starts at `start`, Signal Desktop's rule: its
/// linkify-it set-up (`linkify-it` 5.0.2: `fuzzyLink` on, `fuzzyIP` off, the list [`TLDS`]) and what
/// its `isLinkSneaky` then refuses. A host ([`host_at`]), an optional port ([`port_end`]), the end
/// of the word ([`may_end_host`]), then a path, a query or a fragment, which end and are trimmed as
/// those of an address with its scheme.
///
/// NOT a link: an e-mail address ([`EmailNames`]) and what follows its `@` ([`may_start_host`]); a
/// host of ASCII and other characters together, Desktop's `isLinkSneaky`: `münchen.de`, and also a
/// word of a language without spaces written against a domain (`请看example.org`) that would be the
/// first label otherwise. (A host written with its scheme stays a link, see [`allowed_href`].)
fn bare_domain_at(text: &str, start: usize, names: &mut EmailNames) -> Option<usize> {
    let host_end = host_at(text, start)?;
    let host = &text[start..host_end];
    let mut letters = host.chars().filter(|c| *c != '.');
    if letters.clone().any(|c| c.is_ascii()) && letters.any(|c| !c.is_ascii()) {
        return None;
    }
    let end = port_end(text, host_end);
    if !may_end_host(&text[end..]) || names.holds(text, end) {
        return None;
    }
    if !text[end..].starts_with(['/', '?', '#']) {
        return Some(end);
    }
    let tail = url_tail_end(text, end);
    Some(start + trim_url_end(&text[start..tail]))
}

/// Adds the addresses without a scheme of `body[from..to]` that [`allowed_href`] takes to `out`.
/// Each match and all it looks at stay before `to`, where a URL with its scheme starts. The work is
/// bounded by the length of the text: a host is read from its first label only, every other label
/// has a dot, a hyphen or a letter in front of it, which [`may_start_host`] refuses at once.
fn find_bare_domains(
    body: &str,
    from: usize,
    to: usize,
    names: &mut EmailNames,
    out: &mut Vec<(usize, usize)>,
) {
    let text = &body[..to];
    if !text[from..].contains('.') {
        return;
    }
    let mut at = from;
    while let Some(c) = text[at..].chars().next() {
        if (c.is_ascii_alphanumeric() || (!c.is_ascii() && !is_unseen(c)))
            && may_start_host(&text[..at])
            && let Some(end) = bare_domain_at(text, at, names)
        {
            if allowed_href(&href_of_match(&text[at..end])).is_some() {
                out.push((at, end));
            }
            at = end;
        } else {
            at += c.len_utf8();
        }
    }
}

/// Inline style flags of one segment.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct Style {
    bold: bool,
    italic: bool,
    strike: bool,
    mono: bool,
    spoiler: bool,
}

#[derive(Clone, PartialEq, Eq)]
enum Tag<'a> {
    A(&'a str),
    B,
    I,
    S,
    Code,
}

impl Tag<'_> {
    fn open(&self, out: &mut String) {
        match self {
            Tag::A(href) => {
                out.push_str("<a href=\"");
                escape_into(out, href);
                out.push_str("\">");
            }
            Tag::B => out.push_str("<b>"),
            Tag::I => out.push_str("<i>"),
            Tag::S => out.push_str("<s>"),
            Tag::Code => out.push_str("<code>"),
        }
    }

    fn close(&self, out: &mut String) {
        out.push_str(match self {
            Tag::A(_) => "</a>",
            Tag::B => "</b>",
            Tag::I => "</i>",
            Tag::S => "</s>",
            Tag::Code => "</code>",
        });
    }
}

/// HTML-escapes `s` (text and attribute safe).
pub fn escape_into(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
}

/// HTML-escapes `s` into a new string.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    escape_into(&mut out, s);
    out
}

/// Builds the displayed text and HTML while counting UTF-16 positions.
struct Emitter {
    html: String,
    plain: String,
    pos16: u32,
    last_ws: bool,
    spoilers: Vec<Span16>,
    /// The bidi controls open at this point of the text: where each stands in [`Self::html`], and
    /// whether it is an isolate ([`Self::track_bidi`]).
    open_bidi: Vec<(usize, bool)>,
}

impl Emitter {
    /// Keeps [`Self::open_bidi`] up to date with `c`, which is about to go into the markup, by the
    /// bidi algorithm's own rules: U+202C closes the last embedding or override unless an isolate
    /// was opened after it, U+2069 closes the last isolate and all opened after it. The whole text
    /// is one paragraph for it, also over a line break: `<br/>` does not end Qt's text block.
    fn track_bidi(&mut self, c: char) {
        match c {
            '\u{202A}' | '\u{202B}' | '\u{202D}' | '\u{202E}' => {
                self.open_bidi.push((self.html.len(), false));
            }
            '\u{2066}'..='\u{2068}' => self.open_bidi.push((self.html.len(), true)),
            '\u{202C}' if matches!(self.open_bidi.last(), Some((_, false))) => {
                self.open_bidi.pop();
            }
            '\u{2069}' => {
                if let Some(at) = self.open_bidi.iter().rposition(|(_, isolate)| *isolate) {
                    self.open_bidi.truncate(at);
                }
            }
            _ => {}
        }
    }

    /// A link starts here: no bidi control stays open across it, or it would reorder what the
    /// address reads in the bubble (an override turns even its letters around). In the markup only,
    /// each open one becomes U+2060, the word joiner: it draws nothing and reorders nothing, and it
    /// has the same length in UTF-8 and in UTF-16, so [`Self::pos16`], the spoilers, `plain` and the
    /// copy of the text are what they would be anyway, and so is every offset into the document.
    /// (A selection dragged over it copies the word joiner. A pop further on then closes nothing.)
    fn close_open_bidi(&mut self) {
        for (at, _) in self.open_bidi.drain(..) {
            self.html
                .replace_range(at..at + '\u{2060}'.len_utf8(), "\u{2060}");
        }
    }

    fn push_char(&mut self, c: char, next: Option<char>) {
        match c {
            '\n' | '\u{2028}' | '\u{2029}' => {
                self.html.push_str("<br/>");
                self.plain.push('\n');
                self.pos16 += 1;
                self.last_ws = true;
            }
            '\t' => {
                for _ in 0..4 {
                    self.html.push_str("&nbsp;");
                    self.plain.push(' ');
                    self.pos16 += 1;
                }
                self.last_ws = true;
            }
            ' ' => {
                // A plain space only where HTML would not collapse or drop it.
                let at_line_end = next.is_none_or(|n| n == '\n');
                if self.last_ws || at_line_end {
                    self.html.push_str("&nbsp;");
                } else {
                    self.html.push(' ');
                }
                self.plain.push(' ');
                self.pos16 += 1;
                self.last_ws = true;
            }
            c if c.is_control() || c == MENTION_PLACEHOLDER => {}
            c => {
                self.track_bidi(c);
                let mut buf = [0u8; 4];
                escape_into(&mut self.html, c.encode_utf8(&mut buf));
                self.plain.push(c);
                self.pos16 += c.len_utf16() as u32;
                self.last_ws = false;
            }
        }
    }

    fn push_str(&mut self, s: &str) {
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            self.push_char(c, chars.peek().copied());
        }
    }

    fn mark_spoiler(&mut self, start: u32) {
        if self.pos16 <= start {
            return;
        }
        match self.spoilers.last_mut() {
            Some(last) if last.start + last.len == start => last.len = self.pos16 - last.start,
            _ => self.spoilers.push(Span16 {
                start,
                len: self.pos16 - start,
            }),
        }
    }
}

/// Renders `text` (ranges are validated here again; invalid ones are ignored).
pub fn render(text: &Text, names: &dyn Names) -> RichText {
    render_with(text, names, find_urls(&text.body))
}

/// [`render`] with the links the text holds by itself given as `urls` ([`find_urls`]): a part of a
/// long text links what the whole text links ([`crate::readmore`]).
pub(crate) fn render_with(text: &Text, names: &dyn Names, urls: Vec<(usize, usize)>) -> RichText {
    let body = text.body.as_str();
    let end = body.trim_end().len();
    let ranges: Vec<(usize, usize, &RangeKind)> = text
        .ranges
        .iter()
        .filter(|r| validate_range(body, r).is_ok())
        .map(|r| {
            let s = r.start as usize;
            (s, s + r.len as usize, &r.kind)
        })
        .collect();

    // Links the text holds by itself become links unless they touch an explicit link or code.
    let mut autolinks: Vec<(usize, usize, String)> = Vec::new();
    for (s, e) in urls {
        let blocked = ranges.iter().any(|(rs, re, k)| {
            matches!(k, RangeKind::Link(_) | RangeKind::Mono) && *rs < e && s < *re
        });
        if !blocked {
            autolinks.push((s, e, href_of_match(&body[s..e])));
        }
    }

    // The mentions of the text: an entry per valid mention range (not "everyone": no person).
    let mut mentions: Vec<UserId> = Vec::new();
    let mut mention_of_range: Vec<Option<usize>> = Vec::with_capacity(ranges.len());
    // (One mention per placeholder: `Text::shown_mentions`, the rule the menu's pages use too.)
    let shown = text.shown_mentions();
    for (_, _, kind) in &ranges {
        mention_of_range.push(match kind {
            RangeKind::Mention(user) if shown.iter().any(|w| std::ptr::eq(&w.kind, *kind)) => {
                mentions.push(user.clone());
                Some(mentions.len() - 1)
            }
            _ => None,
        });
    }
    let mention_hrefs: Vec<String> = (0..mentions.len())
        .map(|i| format!("{MENTION_HREF}{i}"))
        .collect();
    let mut mention_spoilers = vec![false; mentions.len()];

    let mut cuts: BTreeSet<usize> = BTreeSet::from([0, end]);
    for (s, e, _) in &ranges {
        cuts.insert((*s).min(end));
        cuts.insert((*e).min(end));
    }
    for (s, e, _) in &autolinks {
        cuts.insert((*s).min(end));
        cuts.insert((*e).min(end));
    }
    let cuts: Vec<usize> = cuts.into_iter().collect();

    let mut em = Emitter {
        html: String::with_capacity(body.len() + 16),
        plain: String::with_capacity(body.len()),
        pos16: 0,
        last_ws: true,
        spoilers: Vec::new(),
        open_bidi: Vec::new(),
    };
    let mut open: Vec<Tag<'_>> = Vec::new();
    let mut links: Vec<String> = Vec::new();
    let mut spoiled: Vec<String> = Vec::new();
    for w in cuts.windows(2) {
        let (a, b) = (w[0], w[1]);
        if a >= b {
            continue;
        }
        let mut style = Style::default();
        let mut link: Option<(usize, &str)> = None;
        let mut mention: Option<(usize, &RangeKind)> = None;
        for (ri, (s, e, kind)) in ranges.iter().enumerate() {
            if *s > a || *e < b {
                continue;
            }
            match kind {
                RangeKind::Bold => style.bold = true,
                RangeKind::Italic => style.italic = true,
                RangeKind::Strike => style.strike = true,
                RangeKind::Mono => style.mono = true,
                RangeKind::Spoiler => style.spoiler = true,
                RangeKind::Link(url) => {
                    // The innermost (shortest) valid link wins.
                    if let Some(href) = allowed_href(url.trim())
                        && link.is_none_or(|(len, _)| e - s < len)
                    {
                        link = Some((e - s, href));
                    }
                }
                RangeKind::Mention(_) | RangeKind::MentionAll => mention = Some((ri, kind)),
            }
        }
        if link.is_none() {
            link = autolinks
                .iter()
                .find(|(s, e, _)| *s <= a && b <= *e)
                .and_then(|(s, e, url)| allowed_href(url).map(|h| (e - s, h)));
        }

        if let Some((_, href)) = link {
            let list = if style.spoiler {
                &mut spoiled
            } else {
                &mut links
            };
            if !list.iter().any(|l| l == href) {
                list.push(href.to_owned());
            }
        }

        // A mention outside any link is a link of its own (P3); inside one it stays that link.
        let mention_link = match mention {
            Some((ri, _)) if link.is_none() => mention_of_range[ri],
            _ => None,
        };
        let mut want: Vec<Tag<'_>> = Vec::with_capacity(5);
        if let Some((_, href)) = link {
            want.push(Tag::A(href));
        } else if let Some(mi) = mention_link {
            want.push(Tag::A(&mention_hrefs[mi]));
        }
        if style.bold || mention.is_some() {
            want.push(Tag::B);
        }
        if style.italic {
            want.push(Tag::I);
        }
        if style.strike {
            want.push(Tag::S);
        }
        if style.mono {
            want.push(Tag::Code);
        }
        let keep = open.iter().zip(&want).take_while(|(o, w)| o == w).count();
        while open.len() > keep {
            if let Some(tag) = open.pop() {
                tag.close(&mut em.html);
            }
        }
        for tag in &want[keep..] {
            // (A mention's link is no address; it stays as the text around it draws it.)
            if matches!(tag, Tag::A(href) if !href.starts_with(MENTION_HREF)) {
                em.close_open_bidi();
            }
            tag.open(&mut em.html);
            open.push(tag.clone());
        }

        let start16 = em.pos16;
        match mention {
            Some((_, kind)) => {
                let name = match kind {
                    RangeKind::Mention(user) => names.display_name(user),
                    _ => "everyone".to_owned(),
                };
                let label: String = name
                    .chars()
                    .map(|c| if c.is_control() { ' ' } else { c })
                    .collect();
                em.push_str(&format!("@{label}"));
            }
            None => {
                let seg = &body[a..b];
                let mut chars = seg.chars().peekable();
                while let Some(c) = chars.next() {
                    let next = chars
                        .peek()
                        .copied()
                        .or_else(|| body[b..end].chars().next());
                    em.push_char(c, next);
                }
            }
        }
        if style.spoiler {
            em.mark_spoiler(start16);
            if let Some(mi) = mention_link {
                mention_spoilers[mi] = true;
            }
        }
    }
    while let Some(tag) = open.pop() {
        tag.close(&mut em.html);
    }

    links.retain(|l| !spoiled.contains(l));
    let jumbo = text.ranges.is_empty() && is_jumbo_emoji(body);
    RichText {
        html: em.html,
        plain: em.plain,
        spoilers: em.spoilers,
        jumbo,
        more: false,
        more_words: 0,
        whole: String::new(),
        links,
        mentions,
        mention_spoilers,
    }
}

/// Renders a plain string (no ranges) the same way: escaped, whitespace kept, links found.
pub fn render_plain(body: &str, names: &dyn Names) -> RichText {
    render(&Text::plain(body), names)
}

fn is_emoji_base(c: char) -> bool {
    matches!(c as u32,
        0x1F000..=0x1FAFF
        | 0x2600..=0x27BF
        | 0x2300..=0x23FF
        | 0x2B00..=0x2BFF
        | 0x2190..=0x21FF
        | 0x3030 | 0x303D | 0x3297 | 0x3299
        | 0x00A9 | 0x00AE | 0x203C | 0x2049 | 0x2122 | 0x2139)
}

fn is_emoji_modifier(c: char) -> bool {
    matches!(c as u32, 0xFE0F | 0x20E3 | 0x1F3FB..=0x1F3FF | 0xE0020..=0xE007F)
}

/// Whether `body` is one to three emoji (and whitespace) only: "big emoji" messages.
pub fn is_jumbo_emoji(body: &str) -> bool {
    let mut clusters = 0u32;
    let mut join = false;
    let mut pending_ri = false;
    let mut any = false;
    for c in body.chars() {
        if c.is_whitespace() {
            join = false;
            pending_ri = false;
            continue;
        }
        if c == '\u{200D}' {
            join = true;
            continue;
        }
        if is_emoji_modifier(c) {
            if !any {
                return false;
            }
            continue;
        }
        if !is_emoji_base(c) {
            return false;
        }
        any = true;
        let regional = matches!(c as u32, 0x1F1E6..=0x1F1FF);
        if join {
            join = false;
        } else if regional && pending_ri {
            pending_ri = false;
        } else {
            clusters += 1;
            pending_ri = regional;
        }
        if clusters > 3 {
            return false;
        }
    }
    any && clusters >= 1
}

#[cfg(test)]
mod tests {
    use super::*;
    use kourier_model::text::{MENTION_PLACEHOLDER, Utf16Map};
    use kourier_model::{TextRange, UserId};

    struct TestNames;
    impl Names for TestNames {
        fn display_name(&self, user: &UserId) -> String {
            match user.value() {
                "olena" => "Olena".into(),
                _ => "<script>".into(),
            }
        }
    }

    fn text(body: &str, ranges: &[(u32, u32, RangeKind)]) -> Text {
        Text {
            body: body.into(),
            ranges: ranges
                .iter()
                .map(|(start, len, kind)| TextRange {
                    start: *start,
                    len: *len,
                    kind: kind.clone(),
                })
                .collect(),
        }
    }

    fn html(body: &str, ranges: &[(u32, u32, RangeKind)]) -> String {
        render(&text(body, ranges), &TestNames).html
    }

    /// Every tag in `html` is one of the whitelist and every `<a` has exactly one attribute.
    fn assert_whitelisted(html: &str) {
        let mut rest = html;
        while let Some(i) = rest.find('<') {
            let after = &rest[i + 1..];
            let close = after.find('>').expect("unterminated tag");
            let tag = &after[..close];
            let ok = matches!(
                tag,
                "b" | "/b" | "i" | "/i" | "s" | "/s" | "code" | "/code" | "br/" | "/a"
            ) || (tag.starts_with("a href=\"")
                && tag.ends_with('"')
                && !tag[8..tag.len() - 1].contains('"'));
            assert!(ok, "tag <{tag}> in {html}");
            rest = &after[close + 1..];
        }
    }

    #[test]
    fn plain_text_is_escaped() {
        let h = html("<img src=x onerror=alert(1)> & \"q\" 'a'", &[]);
        assert_eq!(
            h,
            "&lt;img src=x onerror=alert(1)&gt; &amp; &quot;q&quot; &#39;a&#39;"
        );
        assert_whitelisted(&h);
        let h = html("<b>not bold</b><script>x</script>", &[]);
        assert!(!h.contains("<b>") && !h.contains("<script"), "{h}");
    }

    #[test]
    fn formatting_ranges_nest() {
        // "bold italic plain": bold 0..11, italic 5..11.
        let h = html(
            "bold italic plain",
            &[(0, 11, RangeKind::Bold), (5, 6, RangeKind::Italic)],
        );
        assert_eq!(h, "<b>bold <i>italic</i></b> plain");
        // Overlapping, not nested: bold 0..6, strike 3..9 → tags stay balanced.
        let h = html(
            "abcdefghij",
            &[(0, 6, RangeKind::Bold), (3, 6, RangeKind::Strike)],
        );
        assert_eq!(h, "<b>abc<s>def</s></b><s>ghi</s>j");
        assert_whitelisted(&h);
        let h = html("x y", &[(0, 3, RangeKind::Mono)]);
        assert_eq!(h, "<code>x y</code>");
    }

    /// "Copy Link Address": each link once, in reading order; what a spoiler covers stays hidden.
    #[test]
    fn the_links_of_a_text_are_listed_once_and_a_spoiler_keeps_its_own() {
        let r = render(
            &text("a.de then https://b.example/x and a.de", &[]),
            &TestNames,
        );
        assert_eq!(r.links, ["https://a.de", "https://b.example/x"]);
        let r = render(
            &text(
                "click",
                &[(0, 5, RangeKind::Link("https://c.example/".into()))],
            ),
            &TestNames,
        );
        assert_eq!(r.links, ["https://c.example/"]);
        let body = "see https://secret.example";
        let r = render(&text(body, &[(4, 22, RangeKind::Spoiler)]), &TestNames);
        assert!(r.links.is_empty(), "a link under a spoiler is not offered");
        assert!(
            render(&text("plain words", &[]), &TestNames)
                .links
                .is_empty(),
            "no links, no entries"
        );
    }

    #[test]
    fn links_only_with_allowed_schemes() {
        let link = |url: &str| html("click", &[(0, 5, RangeKind::Link(url.into()))]);
        assert_eq!(
            link("https://example.org/a?b=1&c=\"2\""),
            "<a href=\"https://example.org/a?b=1&amp;c=&quot;2&quot;\">click</a>"
        );
        assert_eq!(
            link("sgnl://signal.me/#p/x"),
            "<a href=\"sgnl://signal.me/#p/x\">click</a>"
        );
        for bad in [
            "javascript:alert(1)",
            "JaVaScRiPt:alert(1)",
            "java\tscript:alert(1)",
            "data:text/html,<b>",
            "file:///etc/passwd",
            "https:",
            "https:///x",
            "http//x",
            "\" onclick=\"x",
            "",
        ] {
            assert_eq!(link(bad), "click", "{bad:?}");
        }
        for (url, ok) in [
            ("HTTPS://EXAMPLE.ORG", true),
            ("http://a", true),
            ("sgnl:", false),
            ("ftp://x", false),
        ] {
            assert_eq!(allowed_href(url).is_some(), ok, "{url}");
        }
    }

    #[test]
    fn bare_urls_are_linked() {
        let h = html("see https://kde.org/plasma, (or http://x.y/a_(b)).", &[]);
        assert_eq!(
            h,
            "see <a href=\"https://kde.org/plasma\">https://kde.org/plasma</a>, (or <a href=\"http://x.y/a_(b)\">http://x.y/a_(b)</a>)."
        );
        assert_eq!(find_urls("nohttp://x shttps://y https://"), vec![]);
        assert_eq!(find_urls("«https://a.b»"), vec![(2, 13)]);
        // Not inside code.
        assert_eq!(
            html("`https://a.b`", &[(1, 11, RangeKind::Mono)]),
            "`<code>https://a.b</code>`"
        );
        // A javascript: "URL" is never linked.
        assert_eq!(html("javascript:alert(1)", &[]), "javascript:alert(1)");
    }

    /// The zero-width non-joiner and joiner: judged by their place, not by the set.
    const ZWNJ: char = '\u{200C}';
    const ZWJ: char = '\u{200D}';
    /// The Persian word for "wiki" and for "pedia" (letters that join): a ZWNJ between them is
    /// part of how the Persian Wikipedia spells its name.
    const FA_WIKI: &str = "\u{0648}\u{06CC}\u{06A9}\u{06CC}";
    const FA_PEDIA: &str = "\u{067E}\u{062F}\u{06CC}\u{0627}";
    /// A Devanagari consonant, the virama, a consonant (a ZWJ goes after the virama).
    const HI_KA: &str = "\u{0915}\u{094D}";
    const HI_SSA: &str = "\u{0937}";
    /// A Malayalam word stem that ends in the virama (a ZWJ after it makes the chillu "n", the
    /// spelling before Unicode 5.1 and of some input methods still) and a Bengali one (a ZWJ
    /// after it makes the khanda ta): both stand at the END of a word.
    const ML_STEM: &str = "\u{0D05}\u{0D35}\u{0D28}\u{0D4D}";
    const BN_STEM: &str = "\u{09A4}\u{09CD}";

    /// The ranges the context-free set names, written out once more (H1, H1b), in the lead's own
    /// grouping (the code merges some of them): first and last character of each. The tests below
    /// take their rows from here.
    const INVISIBLE_OR_BIDI: [(char, char); 18] = [
        ('\u{00AD}', '\u{00AD}'), // soft hyphen
        ('\u{034F}', '\u{034F}'), // combining grapheme joiner
        ('\u{061C}', '\u{061C}'), // Arabic letter mark
        ('\u{115F}', '\u{1160}'), // Hangul choseong and jungseong fillers
        ('\u{17B4}', '\u{17B5}'), // Khmer inherent vowels
        ('\u{180E}', '\u{180E}'), // Mongolian vowel separator
        ('\u{200B}', '\u{200B}'), // zero-width space
        ('\u{200E}', '\u{200F}'), // LRM, RLM
        ('\u{202A}', '\u{202E}'), // LRE, RLE, PDF, LRO, RLO
        // word joiner, invisible function application .. plus, the unassigned U+2065, LRI .. PDI,
        // the deprecated format characters
        ('\u{2060}', '\u{206F}'),
        ('\u{3164}', '\u{3164}'),   // Hangul filler
        ('\u{FEFF}', '\u{FEFF}'),   // zero-width no-break space
        ('\u{FFA0}', '\u{FFA0}'),   // halfwidth Hangul filler
        ('\u{FFF0}', '\u{FFFB}'),   // the unassigned U+FFF0..U+FFF8, interlinear annotation
        ('\u{1BCA0}', '\u{1BCA3}'), // shorthand format controls
        ('\u{1D173}', '\u{1D17A}'), // musical symbol beams and phrase marks
        ('\u{E0000}', '\u{E00FF}'), // tag characters, then the unassigned U+E0080..U+E00FF
        ('\u{E01F0}', '\u{E0FFF}'), // the unassigned rest of the block, after the variation selectors
    ];

    /// The virama table (Canonical_Combining_Class 9), written out once more: the product's
    /// `VIRAMAS` and the proptest's own copy of the joiner rule are judged against it.
    const VIRAMA_TABLE: [u32; 69] = [
        0x094D, 0x09CD, 0x0A4D, 0x0ACD, 0x0B4D, 0x0BCD, 0x0C4D, 0x0CCD, 0x0D3B, 0x0D3C, 0x0D4D,
        0x0DCA, 0x0E3A, 0x0EBA, 0x0F84, 0x1039, 0x103A, 0x1714, 0x1715, 0x1734, 0x17D2, 0x1A60,
        0x1B44, 0x1BAA, 0x1BAB, 0x1BF2, 0x1BF3, 0x2D7F, 0xA806, 0xA82C, 0xA8C4, 0xA953, 0xA9C0,
        0xAAF6, 0xABED, 0x10A3F, 0x11046, 0x11070, 0x1107F, 0x110B9, 0x11133, 0x11134, 0x111C0,
        0x11235, 0x112EA, 0x1134D, 0x113CE, 0x113CF, 0x113D0, 0x11442, 0x114C2, 0x115BF, 0x1163F,
        0x116B6, 0x1172B, 0x11839, 0x1193D, 0x1193E, 0x119E0, 0x11A34, 0x11A47, 0x11A99, 0x11C3F,
        0x11D44, 0x11D45, 0x11D97, 0x11F41, 0x11F42, 0x1612F,
    ];

    /// Whether `s` holds a joiner whose neighbours are not both non-ASCII characters that
    /// are no white space, control character, joiner or member of `set` (the rule, written
    /// out once more).
    fn joiner_out_of_context(s: &str, set: &[char]) -> bool {
        let cs: Vec<char> = s.chars().collect();
        // Straight after a virama a joiner is in context whatever follows.
        let after_virama = |i: usize| i > 0 && VIRAMA_TABLE.contains(&u32::from(cs[i - 1]));
        let beside = |j: Option<usize>| {
            j.and_then(|j| cs.get(j)).is_some_and(|n| {
                u32::from(*n) >= 0x80
                    && !n.is_whitespace()
                    && !n.is_control()
                    && *n != ZWNJ
                    && *n != ZWJ
                    && !set.contains(n)
            })
        };
        cs.iter().enumerate().any(|(i, c)| {
            (*c == ZWNJ || *c == ZWJ)
                && !(after_virama(i) || (beside(i.checked_sub(1)) && beside(Some(i + 1))))
        })
    }

    /// Every character of [`INVISIBLE_OR_BIDI`]: 3,915 of them.
    fn invisible_chars() -> Vec<char> {
        INVISIBLE_OR_BIDI
            .iter()
            .flat_map(|(first, last)| *first..=*last)
            .collect()
    }

    /// The characters that hide in an address when ASCII stands beside them: the set and the two
    /// joiners.
    fn hiding_chars() -> Vec<char> {
        let mut all = invisible_chars();
        all.extend([ZWNJ, ZWJ]);
        all
    }

    /// The four Hangul fillers are LETTERS for `char::is_alphanumeric`, so one straight before
    /// `https://` is no word boundary (as any letter is none): no link, not a longer one.
    const FILLER_LETTERS: [char; 4] = ['\u{115F}', '\u{1160}', '\u{3164}', '\u{FFA0}'];

    fn anchor(url: &str) -> String {
        format!("<a href=\"{url}\">{url}</a>")
    }

    /// H1, H1b, FIX-2: the helper names exactly the 3,915 characters of the eighteen ranges: not one more
    /// (the white space, the variation selectors, the two joiners and the neighbours of each range
    /// stay out), not one less. Every one of them is a character `is_control` and `is_whitespace`
    /// do not name, which is why the helper exists.
    #[test]
    fn the_invisible_and_bidi_characters_are_exactly_these() {
        let expected = invisible_chars();
        assert_eq!(expected.len(), 3915);
        let found: Vec<char> = (0..=u32::from(char::MAX))
            .filter_map(char::from_u32)
            .filter(|c| is_invisible_or_bidi(*c))
            .collect();
        assert_eq!(found, expected);
        for c in expected {
            assert!(
                !c.is_control() && !c.is_whitespace(),
                "U+{:04X}",
                u32::from(c)
            );
        }
        // The unassigned default-ignorables (Qt draws them as nothing) are on it, at the first and
        // last code point of each block.
        for c in [
            '\u{2065}',
            '\u{FFF0}',
            '\u{FFF8}',
            '\u{E0080}',
            '\u{E00FF}',
            '\u{E01F0}',
            '\u{E0FFF}',
        ] {
            assert!(is_invisible_or_bidi(c), "U+{:04X}", u32::from(c));
        }
        // (Not on the list: white space, a hyphen, the joiners, the variation selectors (an emoji
        // domain carries U+FE0F), the Mongolian free variation selectors, and the code point on
        // each side of every range.)
        for c in [
            '\u{2029}',
            '\u{202F}',
            '\u{2010}',
            '\u{FEFE}',
            'a',
            ZWNJ,
            ZWJ,
            '\u{FE00}',
            '\u{FE0E}',
            '\u{FE0F}',
            '\u{E0100}',
            '\u{E01EF}',
            '\u{180B}',
            '\u{180C}',
            '\u{180D}',
            '\u{180F}',
            '\u{00AC}',
            '\u{00AE}',
            '\u{034E}',
            '\u{0350}',
            '\u{115E}',
            '\u{1161}',
            '\u{17B3}',
            '\u{17B6}',
            '\u{200A}',
            '\u{2070}',
            '\u{3163}',
            '\u{3165}',
            '\u{FFA1}',
            '\u{FFEF}',
            '\u{FFFC}',
            '\u{1BC9F}',
            '\u{1BCA4}',
            '\u{1D172}',
            '\u{1D17B}',
            '\u{DFFFF}',
            '\u{E1000}',
            '\u{205F}',
        ] {
            assert!(!is_invisible_or_bidi(c), "U+{:04X}", u32::from(c));
        }
    }

    /// FIX-2: the virama table of the product is exactly the code points with Canonical_Combining_Class
    /// 9 (Unicode 17, Qt 6.11.1's data), written out once more; none is ASCII, white space,
    /// control, a joiner or in the context-free set.
    #[test]
    fn the_viramas_are_exactly_these() {
        assert_eq!(VIRAMA_TABLE.len(), 69);
        assert!(VIRAMA_TABLE.windows(2).all(|w| w[0] < w[1]), "sorted");
        let found: Vec<u32> = (0..=u32::from(char::MAX))
            .filter_map(char::from_u32)
            .filter(|c| is_virama(*c))
            .map(u32::from)
            .collect();
        assert_eq!(found, VIRAMA_TABLE);
        for u in VIRAMA_TABLE {
            let c = char::from_u32(u).expect("a char");
            assert!(
                !c.is_ascii()
                    && !c.is_whitespace()
                    && !c.is_control()
                    && !is_joiner(c)
                    && !is_invisible_or_bidi(c),
                "U+{u:04X}"
            );
        }
        // The signs that look alike but have another class stay out: a vowel sign, a nukta, the
        // Malayalam letter next to the two Malayalam viramas.
        for c in [
            '\u{093C}', '\u{094C}', '\u{09CC}', '\u{0D3A}', '\u{0D4C}', '\u{0D4E}',
        ] {
            assert!(!is_virama(c), "U+{:04X}", u32::from(c));
        }
    }

    /// H1b: `hides_in_link` is the set's answer for every character but the two joiners, whatever
    /// stands beside it; a joiner is hidden unless both neighbours are non-ASCII characters that
    /// are no white space, control character, joiner or member of the set.
    #[test]
    fn a_joiner_hides_in_a_link_only_out_of_context() {
        let persian = Some('\u{06CC}');
        let sides = [None, Some('a'), Some('/'), persian, Some(' ')];
        // Every other character: the set, whatever stands beside it.
        for c in invisible_chars() {
            for prev in sides {
                for next in sides {
                    assert!(hides_in_link(prev, c, next), "U+{:04X}", u32::from(c));
                }
            }
        }
        for c in [
            'a',
            '/',
            '.',
            ' ',
            '\u{06CC}',
            '\u{FE0F}',
            '\u{180B}',
            '\u{1F468}',
        ] {
            for prev in sides {
                for next in sides {
                    assert!(!hides_in_link(prev, c, next), "U+{:04X}", u32::from(c));
                }
            }
        }
        // A joiner: kept between two non-ASCII characters that are not hidden themselves ...
        let virama = Some('\u{094D}');
        for (prev, next) in [
            (persian, Some('\u{067E}')),            // Arabic-script letters
            (virama, Some('\u{0937}')),             // after a virama
            (Some('\u{1F468}'), Some('\u{1F469}')), // emoji
            (Some('\u{FE0F}'), Some('\u{1F525}')),  // an emoji selector, then an emoji
            (Some('\u{00E9}'), Some('\u{00E9}')),   // non-ASCII Latin letters
            (Some('\u{0301}'), Some('\u{00E9}')),   // a combining mark
            (Some('\u{FFFC}'), persian),            // (the mention mark is no hidden character)
            // ... and, straight after a virama, whatever follows or nothing (the legacy
            // Malayalam chillu and Bengali khanda ta end a word with virama + joiner).
            (virama, None),
            (Some('\u{0D4D}'), None),
            (Some('\u{09CD}'), None),
            (Some('\u{11046}'), None), // a virama outside the BMP
            (virama, Some('a')),
            (virama, Some('%')),
            (virama, Some(' ')),
            (virama, Some('\n')),
            (virama, Some('\u{0090}')),
            (virama, Some('\u{200B}')), // (the set ends the URL there, the joiner stays)
            (virama, Some(ZWJ)),
            (virama, Some(ZWNJ)),
        ] {
            for joiner in [ZWNJ, ZWJ] {
                assert!(!hides_in_link(prev, joiner, next), "{prev:?} {next:?}");
            }
        }
        // ... and hidden anywhere else, also straight after a sign that is no virama.
        for (prev, next) in [
            (None, persian),
            (persian, None),
            (None, None),
            (Some('a'), persian),
            (persian, Some('a')),
            (Some('/'), persian),
            (persian, Some('%')),
            (persian, Some(' ')),
            (persian, Some('\n')),
            (persian, Some('\u{00A0}')), // no-break space
            (persian, Some('\u{3000}')), // ideographic space
            (persian, Some('\u{0085}')), // next line: control and white space
            (persian, Some('\u{0090}')), // a control that is no white space
            (Some('\u{0090}'), persian),
            (Some(ZWJ), persian), // another joiner beside it
            (persian, Some(ZWNJ)),
            (persian, Some('\u{200B}')), // a member of the set beside it
            (Some('\u{E0041}'), persian),
            (persian, Some('\u{00AD}')),
            (Some('\u{202E}'), persian),
            (Some('\u{093C}'), None), // a nukta (class 7), a vowel sign, a letter: no virama
            (Some('\u{094C}'), None),
            (Some('\u{0D3A}'), None),
            (Some('\u{0D4C}'), None),
            (Some('\u{09A4}'), None),
        ] {
            for joiner in [ZWNJ, ZWJ] {
                assert!(hides_in_link(prev, joiner, next), "{prev:?} {next:?}");
            }
        }
    }

    /// H1, H1b: a URL ends before an invisible or bidi character (and before a joiner with ASCII
    /// beside it), which stays in the text, outside the link. One row per character of every
    /// range, at the end of a path, inside a host, straight after the scheme and in front of the
    /// URL.
    #[test]
    fn a_url_ends_at_an_invisible_or_bidi_character() {
        for c in hiding_chars() {
            let u = u32::from(c);
            // The override after the path: "https://a.example/" is the link, the rest is text.
            let body = format!("see https://a.example/{c}tail end");
            assert_eq!(find_urls(&body), vec![(4, 22)], "U+{u:04X}");
            let want_html = format!("see {}{c}tail end", anchor("https://a.example/"));
            for r in [
                render(&Text::plain(&body), &TestNames),
                render_plain(&body, &TestNames),
            ] {
                assert_eq!(r.html, want_html, "U+{u:04X}");
                assert_eq!(r.plain, body, "U+{u:04X}: the character stays in the text");
            }

            // Inside the host: the part before it is the link, the rest is not one.
            let body = format!("https://a.exam{c}ple.test/x");
            assert_eq!(find_urls(&body), vec![(0, 14)], "U+{u:04X}");
            assert_eq!(
                html(&body, &[]),
                format!("{}{c}ple.test/x", anchor("https://a.exam")),
                "U+{u:04X}"
            );

            // Straight after the scheme there is no URL.
            let body = format!("https://{c}a.example/");
            assert_eq!(find_urls(&body), vec![], "U+{u:04X}");
            assert!(!html(&body, &[]).contains("<a "), "U+{u:04X}");

            // In front of the URL: the URL is as long as without it (a letter in front is no
            // word boundary, so a filler that is a letter leaves no link at all).
            let body = format!("{c}https://a.example/");
            if FILLER_LETTERS.contains(&c) {
                assert_eq!(find_urls(&body), vec![], "U+{u:04X}");
            } else {
                assert_eq!(
                    find_urls(&body),
                    vec![(c.len_utf8(), c.len_utf8() + 18)],
                    "U+{u:04X}"
                );
                // (A bidi control left open in front of a link is neutral in the markup, see
                // `a_bidi_control_left_open_before_a_link_does_not_reach_it`.)
                let front = if BIDI_OPENERS.contains(&c) {
                    NEUTRAL
                } else {
                    c
                };
                assert_eq!(
                    html(&body, &[]),
                    format!("{front}{}", anchor("https://a.example/")),
                    "U+{u:04X}"
                );
            }
        }
        // (Nothing else changed: a URL with no such character is as long as it was.)
        assert_eq!(find_urls("https://a.example/x?y=1#z next"), vec![(0, 25)]);

        // FIX-2: the unassigned default-ignorables, which Qt draws as nothing, in a query: the
        // reader sees "alice", the address says another name. The link ends before them.
        let head = "https://a.example/pay?to=al";
        for c in [
            '\u{2065}',
            '\u{FFF0}',
            '\u{FFF8}',
            '\u{E0080}',
            '\u{E00FF}',
            '\u{E01F0}',
            '\u{E0FFF}',
        ] {
            let u = u32::from(c);
            let body = format!("{head}{c}ice");
            assert_eq!(find_urls(&body), vec![(0, head.len())], "U+{u:04X}");
            assert_eq!(
                html(&body, &[]),
                format!("{}{c}ice", anchor(head)),
                "U+{u:04X}"
            );
            assert_eq!(allowed_href(&body), None, "U+{u:04X}");
        }
    }

    /// H1b: a ZWNJ or ZWJ between two non-ASCII letters (a Persian word, an Indic conjunct, an
    /// emoji sequence) is part of the address: ONE link whose `href` holds it, in a bare URL, in
    /// a host and in a link range.
    #[test]
    fn a_joiner_between_non_ascii_characters_stays_in_the_link() {
        let fa_path = format!("https://fa.wikipedia.org/wiki/{FA_WIKI}{ZWNJ}{FA_PEDIA}");
        // (an Iranian host: "I want" with its ZWNJ after the prefix, under .ir)
        let fa_host =
            format!("https://\u{0645}\u{06CC}{ZWNJ}\u{062E}\u{0648}\u{0627}\u{0647}\u{0645}.ir/");
        let hi = format!("https://a.example/{HI_KA}{ZWJ}{HI_SSA}");
        let family = format!("https://a.example/\u{1F468}{ZWJ}\u{1F469}{ZWJ}\u{1F467}");
        let heart = format!("https://a.example/\u{2764}\u{FE0F}{ZWJ}\u{1F525}");
        let query = format!("https://a.example/x?q={FA_WIKI}{ZWJ}{FA_PEDIA};r=1");
        for (url, joiners) in [
            (fa_path, 1),
            (fa_host, 1),
            (hi, 1),
            (family, 2),
            (heart, 1),
            (query, 1),
        ] {
            let body = format!("see {url} end");
            assert_eq!(find_urls(&body), vec![(4, 4 + url.len())], "{url:?}");
            assert_eq!(
                html(&body, &[]),
                format!("see {} end", anchor(&url)),
                "{url:?}"
            );
            let r = render_plain(&body, &TestNames);
            assert_eq!(r.html, format!("see {} end", anchor(&url)), "{url:?}");
            assert_eq!(url.matches([ZWNJ, ZWJ]).count(), joiners, "{url:?}");
            // The address is also taken as a link range (a sender's address for a label).
            assert_eq!(allowed_href(&url), Some(url.as_str()), "{url:?}");
            assert_eq!(
                html("click", &[(0, 5, RangeKind::Link(url.clone()))]),
                format!("<a href=\"{url}\">click</a>"),
                "{url:?}"
            );
            // Punctuation after it is trimmed as before; the joiner stays.
            let body = format!("({url}).");
            assert_eq!(find_urls(&body), vec![(1, 1 + url.len())], "{url:?}");
        }
    }

    /// FIX-2 (LOW-2): a ZWNJ or ZWJ straight after a virama stays in the address whatever follows
    /// it, also at the END of a word, before white space, ASCII or the end of the text: the
    /// legacy Malayalam chillu (consonant, U+0D4D, ZWJ) and the old Bengali khanda ta (U+09A4,
    /// U+09CD, ZWJ) end a word so, Qt takes the host label, and the joiner is what makes the sign
    /// drawn. Every link stays whole in `find_urls`, `render`, `render_plain` and in a link range.
    #[test]
    fn a_joiner_after_a_virama_stays_in_the_link_wherever_it_stands() {
        for joiner in [ZWJ, ZWNJ] {
            let ml = format!("{ML_STEM}{joiner}");
            let bn = format!("{BN_STEM}{joiner}");
            for url in [
                format!("https://ml.example/wiki/{ml}"),
                format!("https://{ml}.example/"),
                format!("https://bn.example/wiki/{bn}"),
                format!("https://bn.example/{ml}/{bn}"),
                format!("https://a.example/x?q={bn}"),
            ] {
                for (before, after) in [
                    ("", ""),
                    ("see ", ""),
                    ("see ", " end"),
                    ("(", ")."),
                    ("", "."),
                    ("", ","),
                    ("", "\n"),
                    ("", "\u{00BB}"),
                    ("", "\u{201D}"),
                    ("", "\u{FFFC}"),
                ] {
                    let body = format!("{before}{url}{after}");
                    let at = before.len();
                    let u = u32::from(joiner);
                    assert_eq!(
                        find_urls(&body),
                        vec![(at, at + url.len())],
                        "U+{u:04X} {body:?}"
                    );
                    for r in [
                        render(&Text::plain(&body), &TestNames),
                        render_plain(&body, &TestNames),
                    ] {
                        assert!(r.html.contains(&anchor(&url)), "U+{u:04X} {body:?}");
                    }
                }
                // The address is also taken as a link range.
                assert_eq!(allowed_href(&url), Some(url.as_str()), "{url:?}");
                assert_eq!(
                    html("click", &[(0, 5, RangeKind::Link(url.clone()))]),
                    format!("<a href=\"{url}\">click</a>"),
                    "{url:?}"
                );
            }
            // Whatever follows the joiner: ASCII (the joiner is after a virama, not beside the
            // letter), and it is the sign's own, so the link goes on.
            let url = format!("https://ml.example/wiki/{ml}x");
            assert_eq!(find_urls(&url), vec![(0, url.len())]);
            assert_eq!(allowed_href(&url), Some(url.as_str()));

            // A hidden character after it ends the link after the joiner; so does another
            // joiner (its neighbour is a joiner, not a virama), the first one stays.
            let url = format!("https://ml.example/wiki/{ml}");
            for tail in [
                "\u{200B}x".to_owned(),
                "\u{202E}x".to_owned(),
                format!("{ZWNJ}{FA_WIKI}"),
                format!("{ZWJ}{FA_WIKI}"),
                ZWNJ.to_string(),
                ZWJ.to_string(),
            ] {
                let body = format!("{url}{tail}");
                assert_eq!(find_urls(&body), vec![(0, url.len())], "{body:?}");
                assert_eq!(render_plain(&body, &TestNames).plain, body, "{body:?}");
            }
        }
    }

    /// H1b: a joiner with ASCII beside it, at the start or the end of an address, beside white
    /// space, a joiner or a hidden character ends the URL before itself (it stays in the text,
    /// outside the link). (The rows put the joiner after a consonant, not after a virama: one
    /// straight after a virama stays, see the test above.)
    #[test]
    fn a_joiner_out_of_context_cuts_the_url() {
        let ka = "\u{0915}";
        let fa = FA_WIKI;
        // (body, the link: its length in bytes from the start of the body)
        let cut = |joiner: char| -> Vec<(String, usize)> {
            let head = "https://a.example/";
            vec![
                // ASCII beside it, either side, and both.
                (format!("https://exam{joiner}ple.test/"), 12),
                (format!("{head}a{joiner}b"), 19),
                (format!("{head}{fa}{joiner}b"), 18 + fa.len()),
                (format!("{head}b{joiner}{fa}"), 19),
                (format!("{head}x?q=1{joiner}"), 23),
                // Straight after the scheme: nothing before it, no URL (0 = none).
                (format!("https://{joiner}{fa}"), 0),
                // At the end of the text and before white space.
                (format!("{head}{ka}{joiner}"), 18 + ka.len()),
                (format!("{head}{ka}{joiner} next"), 18 + ka.len()),
                (format!("{head}{ka}{joiner}\n{fa}"), 18 + ka.len()),
                (format!("{head}{ka}{joiner}\u{00A0}{fa}"), 18 + ka.len()),
                // Before a control character, a mention mark (the URL ends there) and the
                // punctuation a URL does not end with.
                (format!("{head}{ka}{joiner}\u{0090}{fa}"), 18 + ka.len()),
                (
                    format!("{head}{ka}{joiner}{MENTION_PLACEHOLDER}"),
                    18 + ka.len(),
                ),
                (format!("{head}{fa}{joiner}\u{00BB}"), 18 + fa.len()),
                (format!("{head}{fa}{joiner}\u{00BB}."), 18 + fa.len()),
                // Beside another joiner, a member of the set.
                (format!("{head}{fa}{joiner}{ZWNJ}{fa}"), 18 + fa.len()),
                (format!("{head}{fa}{joiner}{ZWJ}{fa}"), 18 + fa.len()),
                (format!("{head}{fa}{joiner}\u{200B}{fa}"), 18 + fa.len()),
                (format!("{head}{fa}\u{E0041}{joiner}{fa}"), 18 + fa.len()),
                (format!("{head}{fa}{joiner}\u{00AD}{fa}"), 18 + fa.len()),
            ]
        };
        for joiner in [ZWNJ, ZWJ] {
            for (body, len) in cut(joiner) {
                let u = u32::from(joiner);
                let want = if len == 0 { vec![] } else { vec![(0, len)] };
                assert_eq!(find_urls(&body), want, "U+{u:04X} {body:?}");
                if len > 0 {
                    let r = render(&Text::plain(&body), &TestNames);
                    assert!(
                        r.html.starts_with(&anchor(&body[..len])),
                        "U+{u:04X} {body:?}"
                    );
                    if body
                        .chars()
                        .all(|c| !c.is_control() && c != MENTION_PLACEHOLDER)
                    {
                        assert_eq!(r.plain, body, "U+{u:04X}: the character stays in the text");
                    }
                    // And the URL found is one `allowed_href` takes.
                    assert_eq!(allowed_href(&body[..len]), Some(&body[..len]), "U+{u:04X}");
                }
            }
        }
    }

    /// H1b: `allowed_href` takes an address whose joiner stands between two non-ASCII characters
    /// and refuses one whose joiner stands anywhere else, for every scheme.
    #[test]
    fn a_link_range_holding_a_joiner_is_a_link_only_in_context() {
        for joiner in [ZWNJ, ZWJ] {
            let fa = format!("{FA_WIKI}{joiner}{FA_PEDIA}");
            for url in [
                format!("https://fa.wikipedia.org/wiki/{fa}"),
                format!("http://a.example/{HI_KA}{joiner}{HI_SSA}"),
                format!("sgnl://signal.me/#p/{fa}"),
                format!("https://{fa}.example/"),
                // After a virama the joiner needs nothing after it (FIX-2).
                format!("https://ml.example/wiki/{ML_STEM}{joiner}"),
                format!("https://{ML_STEM}{joiner}.example/"),
                format!("sgnl://signal.me/#p/{BN_STEM}{joiner}"),
            ] {
                assert_eq!(allowed_href(&url), Some(url.as_str()), "{url:?}");
                assert_eq!(
                    html("click", &[(0, 5, RangeKind::Link(url.clone()))]),
                    format!("<a href=\"{url}\">click</a>")
                );
            }
            for url in [
                format!("https://exam{joiner}ple.test/"),
                format!("https://a.example/a{joiner}b"),
                format!("https://a.example/{FA_WIKI}{joiner}b"),
                format!("https://a.example/b{joiner}{FA_PEDIA}"),
                format!("https://a.example/{FA_WIKI}{joiner}"),
                format!("https://a.example/{FA_WIKI}{joiner}{joiner}{FA_PEDIA}"),
                format!("https://a.example/{FA_WIKI}{joiner}\u{200B}{FA_PEDIA}"),
                format!("https://a.example/{FA_WIKI}\u{E0041}{joiner}{FA_PEDIA}"),
                // Beside a virama but a second joiner (its neighbour is no virama), a hidden
                // character after the first, or no virama before it.
                format!("https://ml.example/wiki/{ML_STEM}{joiner}{joiner}"),
                format!("https://ml.example/wiki/{ML_STEM}{joiner}\u{200B}"),
                format!("https://a.example/{FA_WIKI}\u{093C}{joiner}"),
                format!("https://a.example/{joiner}{ML_STEM}"),
                format!("https://a.example/{FA_WIKI}{joiner}%41"),
                format!("sgnl://signal.me/#p/{FA_WIKI}{joiner}x"),
                format!("{joiner}https://a.example/"),
                format!("https://{joiner}a.example/"),
                format!("  https://a.example/{FA_WIKI}{joiner}  "),
            ] {
                assert_eq!(allowed_href(url.trim()), None, "{url:?}");
                assert_eq!(
                    html("click", &[(0, 5, RangeKind::Link(url.clone()))]),
                    "click",
                    "{url:?}"
                );
            }
        }
    }

    /// H1, H1b: a link RANGE (the sender's address for a label) whose address holds an invisible
    /// or bidi character, or a joiner with ASCII beside it, is no link, anywhere in the address
    /// and for every scheme; the label stays text. The label may hold them (it is only a label;
    /// the dialog shows the real address).
    #[test]
    fn a_link_range_holding_an_invisible_or_bidi_character_is_no_link() {
        for c in hiding_chars() {
            let u = u32::from(c);
            for url in [
                format!("https://a.example/{c}evil"),
                format!("https://a.example/{c}"),
                format!("https://a.exam{c}ple/"),
                format!("{c}https://a.example/"),
                format!("http://a.example/{c}"),
                format!("sgnl://signal.me/#p/{c}x"),
                format!("  https://a.example/{c}  "),
            ] {
                assert_eq!(allowed_href(url.trim()), None, "U+{u:04X} {url:?}");
                assert_eq!(
                    html("click", &[(0, 5, RangeKind::Link(url.clone()))]),
                    "click",
                    "U+{u:04X} {url:?}"
                );
            }
            // A label that holds one, with an address that does not: still a link.
            let label = format!("a{c}b");
            let want = format!("<a href=\"https://a.example/\">{label}</a>");
            assert_eq!(
                html(
                    &label,
                    &[(
                        0,
                        label.len() as u32,
                        RangeKind::Link("https://a.example/".into())
                    )]
                ),
                want,
                "U+{u:04X}"
            );
        }
        // The clean addresses of the same shapes are links.
        for url in [
            "https://a.example/evil",
            "http://a.example/",
            "sgnl://signal.me/#p/x",
        ] {
            assert_eq!(allowed_href(url), Some(url));
            assert_eq!(
                html("click", &[(0, 5, RangeKind::Link(url.into()))]),
                format!("<a href=\"{url}\">click</a>")
            );
        }
    }

    /// LD: one row per rule of a link without a scheme: the text and the links it holds, as written.
    /// (A row of `.example`, `.test` or `.invalid` is one of the reserved names Desktop's list leaves
    /// out; every older test's address uses them, so none of them changed.)
    const BARE_ROWS: &[(&str, &[&str])] = &[
        // A host with a TLD of the list, with `www.` or more labels.
        ("example.org", &["example.org"]),
        ("www.example.org", &["www.example.org"]),
        ("see example.org now", &["example.org"]),
        ("sub.deep.example.org", &["sub.deep.example.org"]),
        ("my-site.example.org", &["my-site.example.org"]),
        ("a.de", &["a.de"]),
        ("fooexample.org", &["fooexample.org"]),
        ("t.me/name", &["t.me/name"]),
        // A port, a path, a query, a fragment.
        ("example.org:8080", &["example.org:8080"]),
        ("example.org:65535/x", &["example.org:65535/x"]),
        ("example.org/path/a.html", &["example.org/path/a.html"]),
        ("example.org/", &["example.org/"]),
        ("example.org?q=1", &["example.org?q=1"]),
        ("example.org#top", &["example.org#top"]),
        ("example.org:8080/x?y=1#z w", &["example.org:8080/x?y=1#z"]),
        ("example.org/a@b.de", &["example.org/a@b.de"]),
        ("example.org/шлях", &["example.org/шлях"]),
        // Any case of a TLD, and the international ones of the list.
        ("EXAMPLE.ORG", &["EXAMPLE.ORG"]),
        ("Example.Org/Path", &["Example.Org/Path"]),
        ("пример.рф", &["пример.рф"]),
        ("ПРИМЕР.РФ", &["ПРИМЕР.РФ"]),
        ("пример.рф/шлях", &["пример.рф/шлях"]),
        ("例子.中国", &["例子.中国"]),
        ("例子.香港", &["例子.香港"]),
        ("مثال.مصر", &["مثال.مصر"]),
        ("ตัวอย่าง.ไทย", &["ตัวอย่าง.ไทย"]),
        ("உதாரணம்.இலங்கை", &["உதாரணம்.இலங்கை"]),
        ("xn--e1afmkfd.xn--p1ai", &["xn--e1afmkfd.xn--p1ai"]),
        // Trailing punctuation and brackets, as for an address with its scheme.
        ("example.org.", &["example.org"]),
        ("example.org,", &["example.org"]),
        ("example.org!", &["example.org"]),
        ("example.org..", &["example.org"]),
        ("example.org: x", &["example.org"]),
        ("example.org:x", &["example.org"]),
        ("(example.org)", &["example.org"]),
        ("(example.org).", &["example.org"]),
        ("example.org/a_(b).", &["example.org/a_(b)"]),
        ("(example.org/a).", &["example.org/a"]),
        ("example.org/a,", &["example.org/a"]),
        ("example.org/?", &["example.org/"]),
        ("example.org?", &["example.org"]),
        ("«example.org»", &["example.org"]),
        ("\"example.org\"", &["example.org"]),
        ("'example.org'", &["example.org"]),
        ("<example.org>", &["example.org"]),
        ("[example.org]", &["example.org"]),
        ("—example.org—", &["example.org"]),
        // What may stand in front.
        ("=example.org", &["example.org"]),
        ("+example.org", &["example.org"]),
        ("$example.org", &["example.org"]),
        ("|example.org", &["example.org"]),
        ("~example.org", &["example.org"]),
        ("x=example.org", &["example.org"]),
        ("\\example.org", &["example.org"]),
        // More than one, and beside an address with its scheme.
        ("a.de, b.fr", &["a.de", "b.fr"]),
        ("a.de,b.fr", &["a.de", "b.fr"]),
        ("a.de/x b.fr/y", &["a.de/x", "b.fr/y"]),
        (
            "x.de https://a.example/ y.de",
            &["x.de", "https://a.example/", "y.de"],
        ),
        ("https://a.example/x.de", &["https://a.example/x.de"]),
        ("x.de/,https://a.example", &["x.de/", "https://a.example"]),
        // Beside an emoji (Desktop looks between emoji).
        ("🔥example.org🔥", &["example.org"]),
        ("❤\u{FE0F}example.org", &["example.org"]),
        ("🔥.de", &[]),
        // False positives Desktop has too (2010's ccTLDs), accepted.
        ("main.rs", &["main.rs"]),
        ("readme.md", &["readme.md"]),
        ("at 10.am", &["10.am"]),
        // Persian, Cyrillic and CJK text around it.
        ("سلام example.org خوبی؟", &["example.org"]),
        ("سلام، example.org؟", &["example.org"]),
        ("«سلام» example.org/شاخه، خوبی", &["example.org/شاخه،"]),
        ("سلام \u{200F}example.org\u{200F} خوبی", &["example.org"]),
        ("سلام مثال.مصر خوبی", &["مثال.مصر"]),
        ("Дивись: example.org/шлях, ось", &["example.org/шлях"]),
        ("дивись example.org.", &["example.org"]),
        ("дивись пример.рф.", &["пример.рф"]),
        ("请看 example.org 。", &["example.org"]),
        ("详情：example.org。", &["example.org"]),
        ("（example.org）", &["example.org"]),
        ("见、example.org，好", &["example.org"]),
        // NOT a TLD of the list: no link.
        ("foo.bar", &[]),
        ("e.g", &[]),
        ("e.g.", &[]),
        ("i.e. this", &[]),
        ("x.y", &[]),
        ("a.example", &[]),
        ("a.test", &[]),
        ("a.invalid", &[]),
        ("a.localhost", &[]),
        ("localhost", &[]),
        ("example.dev", &[]),
        ("example.app", &[]),
        ("notes.txt", &[]),
        ("archive.zip", &[]),
        ("org", &[]),
        (".org", &[]),
        ("1.5", &[]),
        ("v2.0.1", &[]),
        // Inside a word, and after a letter that is no host's, `_` or `@`.
        ("foo.example.orgbar", &[]),
        ("example.orgbar", &[]),
        ("foo_example.org", &[]),
        ("_example.org", &[]),
        ("example.org_", &[]),
        ("example.org_x", &[]),
        ("example.org-", &[]),
        ("example.org-x", &[]),
        ("example.org+x", &[]),
        ("example.org=x", &[]),
        ("example.org~", &[]),
        ("example.org.x", &[]),
        ("example.org.-", &[]),
        ("яexample.org", &[]),
        ("请看example.org。", &[]),
        ("سلامexample.org", &[]),
        // What may not stand in front.
        (".example.org", &[]),
        ("...example.org", &[]),
        ("/example.org", &[]),
        ("//example.org", &[]),
        (":example.org", &[]),
        ("see:example.org", &[]),
        ("-example.org", &[]),
        ("ftp://example.org/x", &[]),
        ("file:example.org", &[]),
        ("hxxp://example.org", &[]),
        ("lib/foo.so", &[]),
        // Labels.
        ("-foo.de", &[]),
        ("foo-.de", &[]),
        ("foo..de", &[]),
        ("foo.-bar.de", &[]),
        // A port that is none.
        ("example.org:99999", &[]),
        ("example.org:123456", &[]),
        ("example.org:00080", &[]),
        ("example.org:8080:1", &[]),
        ("example.org:80-", &[]),
        // An e-mail address, whole; no part of it is a link.
        ("a@example.org", &[]),
        ("see a@example.org now", &[]),
        ("first.last@example.org", &[]),
        ("john.de@example.org", &[]),
        ("john.de@sub.example.org", &[]),
        ("see john.de@example.org, ok", &[]),
        ("<john.de@example.org>", &[]),
        ("[john.de@example.org]", &[]),
        ("(john.de@example.org)", &[]),
        ("mailto:john.de@example.org", &[]),
        ("me+john.de@example.org", &[]),
        ("a.de,john.de@example.org", &[]),
        ("user.de:secret@example.org", &[]),
        ("john.de@example.org:80", &[]),
        ("пошта@example.org", &[]),
        ("john.de\u{200B}@example.org", &[]),
        ("a@\u{200B}example.org", &[]),
        ("a@\u{2066}example.org\u{2069}", &[]),
        // ... and what follows it is looked at on its own.
        ("a@example.org,x.de", &["x.de"]),
        ("a@example.org x.de", &["x.de"]),
        // `@name` stays text.
        ("@name", &[]),
        ("hi @name how", &[]),
        ("@example.org", &[]),
        ("@name.dev", &[]),
        ("x.de @name", &["x.de"]),
        ("x.de@name", &["x.de"]),
        // No IP address without a scheme.
        ("1.2.3.4", &[]),
        ("192.168.1.1:8080/x", &[]),
        ("10.0.0.1.", &[]),
        ("[::1]", &[]),
        // A host of ASCII and other characters together (Desktop's `isLinkSneaky`).
        ("münchen.de", &[]),
        ("пример.com", &[]),
        ("example.рф", &[]),
        ("www.пример.рф", &[]),
        // H1: what hides in an address ends the link, a reader's word is judged past it.
        ("\u{202E}example.org", &["example.org"]),
        ("\u{200E}example.org\u{200E}", &["example.org"]),
        ("example.org\u{202E}/evil", &["example.org"]),
        ("example.org\u{200B} next", &["example.org"]),
        ("example.org/\u{200B}x", &["example.org/"]),
        ("example.org/a\u{202E}b", &["example.org/a"]),
        ("example.org/a\u{200D}b", &["example.org/a"]),
        ("exam\u{200B}ple.org", &[]),
        ("exam\u{200D}ple.org", &[]),
        ("exam\u{00AD}ple.org", &[]),
        // L3: a variation selector draws nothing after a letter (one word), but ends an emoji.
        ("exam\u{FE0F}ple.org", &[]),
        ("\u{2764}\u{FE0F}example.org", &["example.org"]),
        ("example\u{200B}.org", &[]),
        ("example.\u{200B}org", &[]),
        ("example.org\u{200B}bar", &[]),
        ("example.org\u{200B}.x", &[]),
        ("example.org\u{200B}:80", &[]),
        ("\u{2066}x\u{2069}example.org", &[]),
        // The joiner rule: in context it stays, in a host and in a path.
        ("مثال\u{200C}ها.مصر", &["مثال\u{200C}ها.مصر"]),
        ("مثال\u{200D}.مصر", &[]),
        (
            "example.org/مثال\u{200C}ها",
            &["example.org/مثال\u{200C}ها"],
        ),
        ("🔥\u{200D}пример.рф", &["пример.рф"]),
        // A mention's mark stands for "@Name".
        ("\u{FFFC}example.org", &[]),
        ("example.org\u{FFFC}", &[]),
        ("example.org/x\u{FFFC}", &["example.org/x"]),
        // Only http and https; a box-drawing character looks like a slash (Desktop's `isLinkSneaky`).
        ("javascript:example.org", &[]),
        ("sgnl://example.org", &[]),
        ("example.org/a\u{2571}b", &[]),
    ];

    /// What a match opens, as `href` in the markup.
    fn href_for(link: &str) -> String {
        let lower = link.to_ascii_lowercase();
        if lower.starts_with("https://") || lower.starts_with("http://") {
            link.to_owned()
        } else {
            format!("https://{link}")
        }
    }

    /// LD: the table: what is found, that each is a link that opens `https://` and the match, that
    /// nothing else is one, and that the text stays what he wrote.
    #[test]
    fn bare_domains_become_links_as_in_signal_desktop() {
        for (body, links) in BARE_ROWS {
            let found: Vec<&str> = find_urls(body)
                .into_iter()
                .map(|(start, end)| &body[start..end])
                .collect();
            assert_eq!(found, *links, "{body:?}");
            for r in [
                render(&Text::plain(*body), &TestNames),
                render_plain(body, &TestNames),
            ] {
                assert_whitelisted(&r.html);
                assert_eq!(r.html.matches("<a ").count(), links.len(), "{body:?}");
                for link in *links {
                    let want = format!(
                        "<a href=\"{}\">{}</a>",
                        escape(&href_for(link)),
                        escape(link)
                    );
                    assert!(r.html.contains(&want), "{body:?}: {}", r.html);
                }
                let shown: String = body.chars().filter(|c| *c != '\u{FFFC}').collect();
                assert_eq!(r.plain, shown, "{body:?}");
            }
        }
    }

    /// LD: the `href` is `https://` and what he wrote, the text stays as written, in any case.
    #[test]
    fn a_bare_domain_opens_as_https() {
        assert_eq!(
            html("see example.org/a?b=1&c=2.", &[]),
            "see <a href=\"https://example.org/a?b=1&amp;c=2\">example.org/a?b=1&amp;c=2</a>."
        );
        assert_eq!(
            html("WWW.Example.ORG", &[]),
            "<a href=\"https://WWW.Example.ORG\">WWW.Example.ORG</a>"
        );
        assert_eq!(
            html("t.me/name", &[]),
            "<a href=\"https://t.me/name\">t.me/name</a>"
        );
        assert_eq!(
            html("HTTP://example.org", &[]),
            "<a href=\"HTTP://example.org\">HTTP://example.org</a>"
        );
        // Not inside code or a link range; formatting stays around it.
        assert_eq!(
            html("`example.org`", &[(1, 11, RangeKind::Mono)]),
            "`<code>example.org</code>`"
        );
        assert_eq!(
            html(
                "example.org",
                &[(0, 11, RangeKind::Link("https://kde.org".into()))]
            ),
            "<a href=\"https://kde.org\">example.org</a>"
        );
        assert_eq!(
            html("example.org x", &[(0, 11, RangeKind::Bold)]),
            "<a href=\"https://example.org\"><b>example.org</b></a> x"
        );
        // The offsets of what stands behind it count what he wrote, not the `https://`.
        let r = render(
            &text("a.de 😀 secret", &[(10, 6, RangeKind::Spoiler)]),
            &TestNames,
        );
        assert_eq!(r.plain, "a.de 😀 secret");
        assert_eq!(r.spoilers, vec![Span16 { start: 8, len: 6 }]);
    }

    /// LD: a label holds up to 63 characters; a host may have any number of them.
    #[test]
    fn a_host_label_has_at_most_63_characters() {
        let fits = format!("{}.de", "a".repeat(63));
        assert_eq!(find_urls(&fits), vec![(0, fits.len())]);
        let long = format!("{}.de", "a".repeat(64));
        assert_eq!(find_urls(&long), vec![]);
        let cyrillic = format!("{}.рф", "б".repeat(63));
        assert_eq!(find_urls(&cyrillic), vec![(0, cyrillic.len())]);
        // The same in the middle of a host: no later label is taken instead.
        let middle = format!("x.{}.de", "a".repeat(64));
        assert_eq!(find_urls(&middle), vec![]);
    }

    /// LD: the list is Desktop's: 290 names, sorted so that the search finds them; the ones that a
    /// row above does not name in any case are found too. The table of punctuation is well formed.
    #[test]
    fn the_tld_list_and_the_punctuation_table_are_sorted() {
        assert_eq!(TLDS.len(), 290);
        assert!(TLDS.windows(2).all(|w| w[0] < w[1]), "sorted");
        for tld in TLDS {
            assert!(is_tld(tld), "{tld}");
            assert_eq!(
                tld.chars().flat_map(char::to_lowercase).collect::<String>(),
                tld
            );
        }
        for tld in ["COM", "Org", "РФ", "XN--P1AI", "xn--p1ai", "xn--a-b1"] {
            assert!(is_tld(tld), "{tld}");
        }
        for tld in [
            "",
            "xn--",
            "xn--Ж",
            "dev",
            "app",
            "test",
            "example",
            "invalid",
            "localhost",
            "onion",
            "zip",
        ] {
            assert!(!is_tld(tld), "{tld}");
        }

        assert!(
            PUNCTUATION.windows(2).all(|w| w[0] <= w[1]),
            "sorted, first and last of each range"
        );
        assert_eq!(PUNCTUATION.len() % 2, 0);
        for pair in PUNCTUATION.chunks(2).collect::<Vec<_>>().windows(2) {
            assert!(pair[0][1] + 1 < pair[1][0], "ranges apart");
        }
        for c in [
            '¡',
            '«',
            '»',
            '¿',
            '·',
            '—',
            '–',
            '‘',
            '’',
            '“',
            '”',
            '…',
            '،',
            '؛',
            '؟',
            '٪',
            '۔',
            '।',
            '、',
            '。',
            '「',
            '」',
            '（',
            '）',
            '，',
            '：',
            '？',
            '！',
            '\u{10100}',
        ] {
            assert!(is_punctuation(c), "U+{:04X}", u32::from(c));
        }
        for c in [
            'é',
            'я',
            'س',
            '中',
            'ก',
            '\u{094D}',
            '\u{0301}',
            '€',
            '©',
            '\u{1F525}',
            '\u{00A0}',
            '\u{10FFFF}',
        ] {
            assert!(!is_punctuation(c), "U+{:04X}", u32::from(c));
        }
    }

    /// LD: what Desktop's `isLinkSneaky` refuses and `allowed_href` takes up too: a user name or a
    /// password, an address of more than 4,096 UTF-16 units, an empty host or label, a `%` in a host
    /// or a host of more than 2,048, a box-drawing or block character. What it refuses is no link in
    /// a text and in a link range; it stays plain, and nothing inside it becomes a link.
    #[test]
    fn an_address_desktop_calls_sneaky_is_no_link() {
        let long_path = format!("https://a.example/{}", "p".repeat(4096 - 18));
        assert_eq!(long_path.len(), 4096);
        let too_long_path = format!("{long_path}p");
        // Counted in UTF-16 units: a two-unit character counts twice, a two-byte one once.
        let long_emoji = format!("https://a.example/{}", "\u{1F525}".repeat(2040));
        let long_cyrillic = format!("https://a.example/{}", "я".repeat(4096 - 18));
        let host = |len: usize| {
            let label = format!("{}.", "a".repeat(49));
            let mut host = label.repeat(len / label.len());
            host.push_str(&"b".repeat(len - host.len()));
            format!("https://{host}/")
        };
        for url in [
            "https://bank.example@evil.example",
            "https://bank.example@evil.example/x",
            "https://user:secret@evil.example/",
            "http://@evil.example",
            "https://bank.example:80@evil.example/x",
            "https://bank.example\\@evil.example",
            "HTTPS://bank.example@evil.example",
            "https://:80",
            "https://:80/x",
            "https://?x",
            "https://#x",
            "https://a%2eb.example/",
            "https://a..b/",
            "https://.a/",
            "https://a./",
            "https://a.example/a\u{2500}b",
            "https://a.example/a\u{2571}b",
            "https://a.example/a\u{25FF}b",
            "https://\u{2571}.example/",
            "sgnl://signal.me/#p/\u{2571}",
            too_long_path.as_str(),
            long_emoji.as_str(),
        ] {
            assert_eq!(allowed_href(url), None, "{url}");
            assert_eq!(
                html("click", &[(0, 5, RangeKind::Link(url.into()))]),
                "click",
                "{url}"
            );
        }
        for url in [
            "https://a.example/x@y",
            "https://a.example?x@y",
            "https://a.example#x@y",
            "https://a.example:8080/",
            "https://a.example/%41",
            "https://a.example/?q=%41",
            "https://a.example/a\u{24FF}b",
            "https://a.example/a\u{2600}b",
            "http://a",
            "http://localhost:3000/",
            "http://[::1]:8080/x",
            "http://192.168.1.1:8080/",
            long_path.as_str(),
            long_cyrillic.as_str(),
        ] {
            assert_eq!(allowed_href(url), Some(url), "{url}");
        }
        let fits = host(2048);
        assert_eq!(fits.len(), 8 + 2048 + 1);
        assert_eq!(allowed_href(&fits), Some(fits.as_str()));
        let over = host(2049);
        assert_eq!(allowed_href(&over), None);

        // In a text: no link, its text plain, and nothing inside it is a link either.
        for body in [
            "https://bank.example@evil.example",
            "https://bank.de@evil.de",
            "https://bank.de:80@evil.de/",
            "see https://a.de@b.de/c.de,d.de here",
        ] {
            assert_eq!(find_urls(body), vec![], "{body}");
            assert_eq!(html(body, &[]), body, "{body}");
        }
        assert_eq!(find_urls(&too_long_path), vec![]);
        assert_eq!(html(&too_long_path, &[]), too_long_path);
        assert_eq!(find_urls(&format!("{too_long_path},x.de")), vec![]);
        let over_bare = format!("example.org/{}", "p".repeat(4096));
        assert_eq!(find_urls(&over_bare), vec![]);
    }

    /// LD, the bidi controls that stay open until a pop (Desktop's `shouldLinkifyMessage` takes
    /// all links from a text with U+202C, U+202D or U+202E; here the links stay).
    const BIDI_OPENERS: [char; 7] = [
        '\u{202A}', '\u{202B}', '\u{202D}', '\u{202E}', '\u{2066}', '\u{2067}', '\u{2068}',
    ];
    const NEUTRAL: char = '\u{2060}';

    /// LD: a control left open in front of a link would reorder the link's text in the bubble. In the
    /// markup alone it is a character that draws nothing and reorders nothing, as long in every
    /// measure, so no offset and no text changes. A control that is closed in time, one behind the
    /// link and one inside a link's label stay as they are.
    #[test]
    fn a_bidi_control_left_open_before_a_link_does_not_reach_it() {
        let a = anchor("https://a.example/x");
        for c in BIDI_OPENERS {
            let u = u32::from(c);
            for (body, want) in [
                // Open at the link: on the same line, on a line above, at the start of the text.
                (
                    format!("see {c}more https://a.example/x tail"),
                    format!("see {NEUTRAL}more {a} tail"),
                ),
                (
                    format!("{c}more\nhttps://a.example/x"),
                    format!("{NEUTRAL}more<br/>{a}"),
                ),
                (format!("{c}https://a.example/x"), format!("{NEUTRAL}{a}")),
                // Two links: once is enough, and one opened between them is taken too.
                (
                    format!("{c}a https://a.example/x {c}b https://a.example/x"),
                    format!("{NEUTRAL}a {a} {NEUTRAL}b {a}"),
                ),
                // Behind the link: as it was.
                (
                    format!("https://a.example/x {c}tail"),
                    format!("{a} {c}tail"),
                ),
            ] {
                let r = render_plain(&body, &TestNames);
                assert_eq!(r.html, want, "U+{u:04X}");
                assert_eq!(r.plain, body, "U+{u:04X}: the text stays");
                assert_eq!(r.full_plain(), body, "U+{u:04X}: the copy stays");
            }
            // A link without a scheme and a link range are links the same way.
            assert_eq!(
                html(&format!("{c}example.org"), &[]),
                format!("{NEUTRAL}<a href=\"https://example.org\">example.org</a>"),
                "U+{u:04X}"
            );
            let body = format!("{c}see click");
            let at = body.find("click").expect("the label") as u32;
            assert_eq!(
                html(
                    &body,
                    &[(at, 5, RangeKind::Link("https://a.example/".into()))]
                ),
                format!("{NEUTRAL}see <a href=\"https://a.example/\">click</a>"),
                "U+{u:04X}"
            );
            // No link: as it was.
            assert_eq!(
                html(&format!("{c}plain text"), &[]),
                format!("{c}plain text")
            );
            // Inside a label it reaches no address; the next link's question is its own.
            let body = format!("x{c}y https://a.example/x");
            let want = format!("<a href=\"https://b.example/\">x{c}</a>y {a}");
            let html_got = html(
                &body,
                &[(
                    0,
                    (1 + c.len_utf8()) as u32,
                    RangeKind::Link("https://b.example/".into()),
                )],
            );
            assert_eq!(
                html_got,
                want.replace(&format!("x{c}"), &format!("x{NEUTRAL}")),
                "U+{u:04X}: open at the next link"
            );

            // The offsets: a spoiler behind the link stands where it stands without the control's
            // effect, in UTF-16 units of the text with the control in it.
            let body = format!("{c}see https://a.example/x secret");
            let at = body.find("secret").expect("secret") as u32;
            let r = render(&text(&body, &[(at, 6, RangeKind::Spoiler)]), &TestNames);
            let start = 1 + "see https://a.example/x ".len() as u32;
            assert_eq!(r.spoilers, vec![Span16 { start, len: 6 }], "U+{u:04X}");
            let shown: Vec<u16> = r.plain.encode_utf16().collect();
            assert_eq!(
                String::from_utf16(&shown[start as usize..start as usize + 6]).expect("utf16"),
                "secret"
            );
            // And the markup is as long as with the control itself.
            assert_eq!(
                r.html.len(),
                r.html.replace(NEUTRAL, &c.to_string()).len(),
                "U+{u:04X}"
            );
        }

        let closed = [
            // An embedding or an override closed by its pop, an isolate by its own.
            format!("\u{202E}x\u{202C} {a}"),
            format!("\u{202B}x\u{202C} {a}"),
            format!("\u{2067}x\u{2069} {a}"),
            format!("\u{2066}\u{202E}x\u{2069} {a}"),
            format!("\u{202B}\u{202E}x\u{202C}\u{202C} {a}"),
        ];
        for want in closed {
            let body = want.replace(&a, "https://a.example/x");
            assert_eq!(html(&body, &[]), want, "{body:?}");
        }
        for (body, want) in [
            // A pop closes the last embedding only, the outer one stays open.
            (
                "\u{202B}\u{202E}x\u{202C} https://a.example/x".to_owned(),
                format!("{NEUTRAL}\u{202E}x\u{202C} {a}"),
            ),
            // U+202C does not close an isolate.
            (
                "\u{2067}x\u{202C} https://a.example/x".to_owned(),
                format!("{NEUTRAL}x\u{202C} {a}"),
            ),
            // A pop of nothing closes nothing.
            (
                "\u{202C}\u{2069}\u{202E}x https://a.example/x".to_owned(),
                format!("\u{202C}\u{2069}{NEUTRAL}x {a}"),
            ),
        ] {
            assert_eq!(html(&body, &[]), want, "{body:?}");
        }

        // A name in a mention may hold one too; the mention's own link is no address.
        struct Turned;
        impl Names for Turned {
            fn display_name(&self, _user: &UserId) -> String {
                "\u{202E}name".into()
            }
        }
        let m = MENTION_PLACEHOLDER;
        let body = format!("{m} https://a.example/x");
        let r = render(
            &text(&body, &[(0, 3, RangeKind::Mention(olena_id()))]),
            &Turned,
        );
        assert_eq!(
            r.html,
            format!("<a href=\"kourier:mention/0\"><b>@{NEUTRAL}name</b></a> {a}")
        );
        assert_eq!(r.plain, "@\u{202E}name https://a.example/x");
        let body = format!("{m} x");
        let r = render(
            &text(&body, &[(0, 3, RangeKind::Mention(olena_id()))]),
            &Turned,
        );
        assert_eq!(
            r.html,
            "<a href=\"kourier:mention/0\"><b>@\u{202E}name</b></a> x"
        );
    }

    /// LD: with random texts of bare domains, e-mail addresses, words, punctuation, invisible and
    /// bidi characters, joiners and other scripts, every link's `href` starts with `http://` or
    /// `https://` and holds no character of the set and no joiner out of context; a link's text is
    /// its `href` or what follows `https://` in it, so the shown text stays what he wrote; and
    /// `plain` is the text. It judges by the test's own list and rule, and it must have seen links
    /// without a scheme, also with a joiner, and texts where no link came out of a domain.
    #[test]
    fn fuzz_every_href_of_a_text_is_http_and_shows_nothing_hidden() {
        use proptest::prelude::*;
        let judged = invisible_chars();
        let hidden_in =
            |s: &str| s.chars().any(|c| judged.contains(&c)) || joiner_out_of_context(s, &judged);
        let piece = prop_oneof![
            4 => proptest::sample::select(vec![
                "example.org",
                "www.example.org",
                "t.me/name",
                "a.de:8080/x?y=1#z",
                "EXAMPLE.ORG",
                "пример.рф",
                "例子.中国",
            ])
            .prop_map(str::to_owned),
            3 => proptest::sample::select(vec![
                "a@example.org",
                "john.de@example.org",
                "@name",
                "1.2.3.4",
                "foo.bar",
                "münchen.de",
                "foo.example.orgbar",
                "https://a.de@b.de",
                "main.rs",
            ])
            .prop_map(str::to_owned),
            2 => proptest::sample::select(vec![
                "https://a.example/",
                "http://b.example/x?y=1",
            ])
            .prop_map(str::to_owned),
            3 => proptest::sample::select(vec![
                " ", ".", ",", "(", ")", "/", ":", "@", "_", "-", "=", "«", "»", "。", "،", "\n",
            ])
            .prop_map(str::to_owned),
            2 => proptest::sample::select(vec!["word", "سلام", "дивись", "请看", "🔥"])
                .prop_map(str::to_owned),
            2 => proptest::sample::select(invisible_chars()).prop_map(|c| c.to_string()),
            1 => Just(ZWNJ.to_string()),
            1 => Just(ZWJ.to_string()),
            // Bare domains that hold a joiner in context, in the host and in the path.
            2 => Just(format!("{FA_WIKI}{ZWNJ}{FA_PEDIA}.\u{0645}\u{0635}\u{0631}")),
            1 => Just(format!("example.org/{FA_WIKI}{ZWNJ}{FA_PEDIA}")),
        ];
        let body = proptest::collection::vec(piece, 0..8).prop_map(|v| v.concat());
        let links = std::cell::Cell::new(0usize);
        let bare = std::cell::Cell::new(0usize);
        let bare_joiner = std::cell::Cell::new(0usize);
        let none = std::cell::Cell::new(0usize);
        proptest!(|(body in body)| {
            let r = render_plain(&body, &TestNames);
            assert_whitelisted(&r.html);
            let trimmed = body.trim_end();
            let shown: String = trimmed.chars().filter(|c| !c.is_control() || *c == '\n').collect();
            prop_assert_eq!(&r.plain, &shown);
            let mut rest = r.html.as_str();
            let mut here = 0usize;
            while let Some(i) = rest.find("<a href=\"") {
                let after = &rest[i + 9..];
                let end = after.find('"').expect("a closed href");
                let href = &after[..end];
                let tail = &after[end + 2..];
                let close = tail.find("</a>").expect("a closed link");
                let label = &tail[..close];
                links.set(links.get() + 1);
                here += 1;
                prop_assert!(
                    href.starts_with("https://") || href.starts_with("http://")
                        || href.starts_with("HTTPS://") || href.starts_with("HTTP://"),
                    "a link of another scheme: {:?}", href
                );
                prop_assert!(!hidden_in(href), "an href holds one: {:?} of {:?}", href, body);
                if label != href {
                    prop_assert_eq!(href.strip_prefix("https://"), Some(label));
                    bare.set(bare.get() + 1);
                    if label.contains([ZWNJ, ZWJ]) {
                        bare_joiner.set(bare_joiner.get() + 1);
                    }
                }
                rest = &tail[close + 4..];
            }
            if here == 0 && body.contains('.') {
                none.set(none.get() + 1);
            }
        });
        eprintln!(
            "fuzz LD: {} links, {} without a scheme, {} of them with a joiner, {} texts with a dot and no link",
            links.get(),
            bare.get(),
            bare_joiner.get(),
            none.get()
        );
        // (Over 6 runs: 108 to 140 links, 69 to 90 without a scheme, 34 to 44 of them with a joiner,
        // 85 to 93 texts with no link; the floors are a third of the lowest.)
        assert!(links.get() > 35, "only {} links", links.get());
        assert!(
            bare.get() > 23,
            "only {} links without a scheme",
            bare.get()
        );
        assert!(
            bare_joiner.get() > 11,
            "only {} of them held a joiner",
            bare_joiner.get()
        );
        assert!(none.get() > 28, "only {} texts with no link", none.get());
    }

    #[test]
    fn whitespace_survives_html() {
        let r = render(&Text::plain("a  b\n\n  c\td \nx"), &TestNames);
        assert_eq!(
            r.html,
            "a &nbsp;b<br/><br/>&nbsp;&nbsp;c&nbsp;&nbsp;&nbsp;&nbsp;d&nbsp;<br/>x"
        );
        assert_eq!(r.plain, "a  b\n\n  c    d \nx");
        // Trailing whitespace is dropped, control characters too.
        let r = render(&Text::plain("hi\u{7}\r\n \n"), &TestNames);
        assert_eq!(r.html, "hi");
    }

    #[test]
    fn mentions_expand_and_are_escaped() {
        let body = format!("hi {MENTION_PLACEHOLDER} and {MENTION_PLACEHOLDER}!");
        let p1 = 3u32;
        let p2 = p1 + 3 + 5;
        let olena = UserId::parse("aci:olena").expect("id");
        let evil = UserId::parse("aci:evil").expect("id");
        let r = render(
            &text(
                &body,
                &[
                    (p1, 3, RangeKind::Mention(olena)),
                    (p2, 3, RangeKind::Mention(evil.clone())),
                ],
            ),
            &TestNames,
        );
        assert_eq!(
            r.html,
            "hi <a href=\"kourier:mention/0\"><b>@Olena</b></a> and \
             <a href=\"kourier:mention/1\"><b>@&lt;script&gt;</b></a>!"
        );
        assert_whitelisted(&r.html);
        assert_eq!(r.mentions, vec![olena_id(), evil]);
        assert_eq!(r.plain, "hi @Olena and @<script>!");
        // A placeholder without a mention range disappears.
        let r = render(&Text::plain(format!("a{MENTION_PLACEHOLDER}b")), &TestNames);
        assert_eq!(r.plain, "ab");
    }

    fn olena_id() -> UserId {
        UserId::parse("aci:olena").expect("id")
    }

    /// The mention scheme is made here only: `allowed_href` and the schemes stay as they were.
    #[test]
    fn the_mention_scheme_is_not_a_link_scheme() {
        assert_eq!(ALLOWED_SCHEMES, ["http", "https", "sgnl"]);
        assert_eq!(allowed_href("kourier:mention/0"), None);
        assert_eq!(allowed_href("KOURIER:mention/0"), None);
        assert!(MENTION_HREF.starts_with("kourier:"));
    }

    /// Critic M10: a message cannot make a link that opens a person. Its own link range (or
    /// text) that names `kourier:mention/0` stays plain text, beside a REAL mention at index 0.
    #[test]
    fn a_forged_mention_link_stays_plain_text() {
        let m = MENTION_PLACEHOLDER;
        let body = format!("{m} see kourier:mention/0 and https://kde.org");
        let forged = body.find("kourier:").expect("text");
        let r = render(
            &text(
                &body,
                &[
                    (0, 3, RangeKind::Mention(olena_id())),
                    (
                        forged as u32,
                        "kourier:mention/0".len() as u32,
                        RangeKind::Link("kourier:mention/0".into()),
                    ),
                ],
            ),
            &TestNames,
        );
        assert_eq!(
            r.html,
            "<a href=\"kourier:mention/0\"><b>@Olena</b></a> see kourier:mention/0 and \
             <a href=\"https://kde.org\">https://kde.org</a>"
        );
        // Exactly one link of the scheme: the real mention's.
        assert_eq!(r.html.matches("href=\"kourier:").count(), 1);
        assert_eq!(r.mentions, vec![olena_id()]);
        assert_whitelisted(&r.html);
        // The same with the forged range ALONE (no mention): no link at all of that scheme.
        let r = render(
            &text(
                "kourier:mention/0",
                &[(0, 17, RangeKind::Link("kourier:mention/0".into()))],
            ),
            &TestNames,
        );
        assert_eq!(r.html, "kourier:mention/0");
        assert!(r.mentions.is_empty());
    }

    #[test]
    fn a_mention_inside_a_link_stays_that_link() {
        let m = MENTION_PLACEHOLDER;
        let body = format!("{m} and {m}");
        let r = render(
            &text(
                &body,
                &[
                    (0, 3, RangeKind::Mention(olena_id())),
                    (0, 3, RangeKind::Link("https://kde.org".into())),
                    (8, 3, RangeKind::Mention(olena_id())),
                ],
            ),
            &TestNames,
        );
        assert_eq!(
            r.html,
            "<a href=\"https://kde.org\"><b>@Olena</b></a> and \
             <a href=\"kourier:mention/1\"><b>@Olena</b></a>"
        );
        // (The first mention has its entry, the link never uses it: the indexes follow the ranges.)
        assert_eq!(r.mentions.len(), 2);
    }

    #[test]
    fn a_mention_under_a_spoiler_is_marked() {
        let m = MENTION_PLACEHOLDER;
        let body = format!("{m} and {m} and {m}");
        let r = render(
            &text(
                &body,
                &[
                    (0, 3, RangeKind::Mention(olena_id())),
                    (8, 3, RangeKind::Mention(olena_id())),
                    (8, 3, RangeKind::Spoiler),
                    (16, 3, RangeKind::MentionAll),
                ],
            ),
            &TestNames,
        );
        // The spoiler hides the second only; "everyone" is no person and has no link.
        assert_eq!(r.mentions.len(), 2);
        assert_eq!(r.mention_spoilers, vec![false, true]);
        assert!(!r.html.contains("mention/2"));
        assert!(r.html.ends_with("<b>@everyone</b>"), "{}", r.html);
        assert_eq!(r.spoilers.len(), 1);
    }

    #[test]
    fn spoilers_are_utf16_ranges_of_displayed_text() {
        // "😀 secret 👨‍👩‍👧 end", spoiler over "secret 👨‍👩‍👧".
        let fam = "👨\u{200D}👩\u{200D}👧";
        let body = format!("😀 secret {fam} end");
        let start = "😀 ".len() as u32;
        let len = ("secret ".len() + fam.len()) as u32;
        let r = render(
            &text(&body, &[(start, len, RangeKind::Spoiler)]),
            &TestNames,
        );
        let map = Utf16Map::new(&body);
        let (s16, l16) = map.range_to_utf16(start, len).expect("map");
        assert_eq!(
            r.spoilers,
            vec![Span16 {
                start: s16,
                len: l16
            }]
        );
        assert_eq!(r.spoilers[0].start, 3, "😀 is two UTF-16 units");
        let shown: Vec<u16> = r.plain.encode_utf16().collect();
        let hidden = String::from_utf16(&shown[3..3 + l16 as usize]).expect("utf16");
        assert_eq!(hidden, format!("secret {fam}"));
        // Spoilers after a mention are shifted by the expanded name.
        let body = format!("{MENTION_PLACEHOLDER} x");
        let olena = UserId::parse("aci:olena").expect("id");
        let r = render(
            &text(
                &body,
                &[
                    (0, 3, RangeKind::Mention(olena)),
                    (4, 1, RangeKind::Spoiler),
                ],
            ),
            &TestNames,
        );
        assert_eq!(r.plain, "@Olena x");
        assert_eq!(r.spoilers, vec![Span16 { start: 7, len: 1 }]);
        // Adjacent spoiler segments merge.
        let r = render(
            &text(
                "abcdef",
                &[
                    (0, 4, RangeKind::Spoiler),
                    (2, 4, RangeKind::Spoiler),
                    (1, 2, RangeKind::Bold),
                ],
            ),
            &TestNames,
        );
        assert_eq!(r.spoilers, vec![Span16 { start: 0, len: 6 }]);
    }

    #[test]
    fn invalid_ranges_are_ignored() {
        let body = "é!";
        let h = html(
            body,
            &[
                (1, 1, RangeKind::Bold),
                (0, 99, RangeKind::Italic),
                (0, 0, RangeKind::Strike),
            ],
        );
        assert_eq!(h, "é!");
    }

    #[test]
    fn jumbo_emoji() {
        for (s, j) in [
            ("😀", true),
            ("😀😀😀", true),
            ("😀😀😀😀", false),
            ("👨\u{200D}👩\u{200D}👧\u{200D}👦", true),
            ("👍🏽 ❤\u{FE0F}", true),
            ("🇺🇦🇺🇦", true),
            ("🇺🇦🇺🇦🇺🇦🇺🇦", false),
            ("hi 😀", false),
            ("", false),
            ("\u{FE0F}", false),
        ] {
            assert_eq!(is_jumbo_emoji(s), j, "{s:?}");
        }
        assert!(render(&Text::plain("🔥"), &TestNames).jumbo);
    }

    /// People pages P3: with random mention ranges, forged `kourier:` link ranges and texts that
    /// say `kourier:mention/N`, no forged link survives and every link of the scheme is a mention the
    /// viewmodel listed: its index is below `mentions.len()`, no index repeats, it wraps the `@` of a
    /// mention (never other text), the users listed are the ones of the ranges, and every other link
    /// is of an allowed scheme.
    #[test]
    fn fuzz_mention_links() {
        use proptest::prelude::*;
        let link = prop_oneof![
            Just("kourier:mention/0".to_owned()),
            Just("KOURIER:mention/1".to_owned()),
            Just(" kourier:mention/2".to_owned()),
            Just("https://x.example/kourier:mention/0".to_owned()),
            Just("https://kde.org".to_owned()),
            Just("sgnl://signal.me/#p/x".to_owned()),
            Just("javascript:kourier:mention/0".to_owned()),
        ];
        let styles = prop_oneof![
            Just(RangeKind::Bold),
            Just(RangeKind::Mono),
            Just(RangeKind::Spoiler),
            link.prop_map(RangeKind::Link),
        ];
        // Per placeholder: 0 a mention of olena, 1 of someone else, 2 "everyone", 3 none.
        let strategy = (
            "([a-z ]|\u{FFFC}|kourier:mention/[0-9]|https://kde\\.org){0,10}",
            proptest::collection::vec(0u8..4, 12),
            // A second range on the same placeholder (one in four: none), so that two ranges
            // on one placeholder happen.
            proptest::collection::vec(0u8..4, 12),
            proptest::collection::vec((0u32..60, 0u32..30, styles), 0..6),
        );
        proptest!(|((body, picks, again, extra) in strategy)| {
            let other = UserId::parse("aci:other").expect("id");
            let mut ranges: Vec<(u32, u32, RangeKind)> = Vec::new();
            let mut placed = 0usize;
            let mut users = Vec::new();
            let mut placeholders = 0usize;
            for (n, (at, _)) in body.match_indices(MENTION_PLACEHOLDER).enumerate() {
                placeholders += 1;
                for pick in [picks[n % picks.len()], again[n % again.len()]] {
                    let kind = match pick {
                        0 => Some(RangeKind::Mention(olena_id())),
                        1 => Some(RangeKind::Mention(other.clone())),
                        2 => Some(RangeKind::MentionAll),
                        _ => None,
                    };
                    if let Some(kind) = kind {
                        if let RangeKind::Mention(u) = &kind {
                            placed += 1;
                            users.push(u.clone());
                        }
                        ranges.push((at as u32, MENTION_PLACEHOLDER.len_utf8() as u32, kind));
                    }
                }
            }
            ranges.extend(extra);
            let r = render(&text(&body, &ranges), &TestNames);
            assert_whitelisted(&r.html);
            prop_assert_eq!(r.mentions.len(), r.mention_spoilers.len());
            // (Every valid mention range has an entry, whatever links or spoilers lie over it.)
            prop_assert!(r.mentions.len() <= placed);
            // One person per placeholder, whatever was stored on it.
            prop_assert!(r.mentions.len() <= placeholders);
            prop_assert!(r.mentions.iter().all(|u| users.contains(u)));
            let mut seen = Vec::new();
            let mut rest = r.html.as_str();
            while let Some(i) = rest.find("<a href=\"") {
                let after = &rest[i + 9..];
                let end = after.find('"').expect("a closed href");
                let href = &after[..end];
                let tail = &after[end + 2..];
                if let Some(index) = href.strip_prefix(MENTION_HREF) {
                    let n: usize = index.parse().expect("an index");
                    prop_assert!(n < r.mentions.len(), "index {} of {}", n, r.mentions.len());
                    prop_assert!(!seen.contains(&n), "index {} twice", n);
                    seen.push(n);
                    // (The mention's own tags may nest: bold always, with italic, strike or code from its ranges.)
                    let mut inner = tail;
                    while let Some(next) = ["<b>", "<i>", "<s>", "<code>"].iter().find_map(|t| inner.strip_prefix(t)) {
                        inner = next;
                    }
                    prop_assert!(inner.starts_with('@'), "a mention link around other text: {}", tail);
                } else {
                    let scheme = href.split(':').next().unwrap_or("").to_ascii_lowercase();
                    prop_assert!(ALLOWED_SCHEMES.contains(&scheme.as_str()), "a link of the scheme {}", scheme);
                }
                rest = tail;
            }
            prop_assert!(seen.len() <= r.mentions.len());
        });
    }

    /// Random bodies with random (possibly invalid) ranges never produce markup outside the
    /// whitelist, and spoiler ranges stay inside the displayed text.
    #[test]
    fn fuzz_whitelist() {
        use proptest::prelude::*;
        let kinds = prop_oneof![
            Just(RangeKind::Bold),
            Just(RangeKind::Italic),
            Just(RangeKind::Strike),
            Just(RangeKind::Mono),
            Just(RangeKind::Spoiler),
            Just(RangeKind::Link("https://ok.example/\"><img>".into())),
            Just(RangeKind::Link("javascript:x".into())),
            Just(RangeKind::MentionAll),
        ];
        let strategy = (
            "[a-z<>&\"' \\n😀é\u{FFFC}]{0,24}",
            proptest::collection::vec((0u32..30, 0u32..30, kinds), 0..6),
        );
        proptest!(|((body, ranges) in strategy)| {
            let r = render(&text(&body, &ranges), &TestNames);
            assert_whitelisted(&r.html);
            let len16 = r.plain.encode_utf16().count() as u32;
            for s in &r.spoilers {
                prop_assert!(s.len > 0 && s.start + s.len <= len16);
            }
        });
    }

    /// H1, H1b: with random bodies made of URLs, words, invisible or bidi characters, joiners and
    /// non-ASCII letters, and with link ranges (a whole-body one and random ones) whose addresses
    /// hold them, no `href` of `render` or `render_plain` holds a character of the set or a joiner
    /// out of context, and the hrefs that exist are of an allowed scheme. It judges by the test's
    /// own list and its own copy of the joiner rule (`hidden_in`), never by the helpers under
    /// test, and it must have seen both sides: joiners that stayed in a link, and addresses that
    /// only the joiner rule refuses.
    #[test]
    fn fuzz_no_href_holds_an_invisible_or_bidi_character() {
        use proptest::prelude::*;
        let judged = invisible_chars();

        let set_in = |s: &str| s.chars().any(|c| judged.contains(&c));
        let hidden_in = |s: &str| set_in(s) || joiner_out_of_context(s, &judged);

        let letters: Vec<String> = vec![
            "\u{06CC}".into(),
            "\u{067E}".into(),
            HI_KA.into(),
            HI_SSA.into(),
            "\u{1F468}".into(),
            "\u{FE0F}".into(),
            "\u{00E9}".into(),
        ];
        let piece = prop_oneof![
            3 => Just("https://a.example/".to_owned()),
            2 => Just("http://b.example/x?y=1".to_owned()),
            1 => Just("sgnl://signal.me/#p/x".to_owned()),
            2 => Just(" ".to_owned()),
            2 => Just("word".to_owned()),
            1 => Just(".)".to_owned()),
            3 => proptest::sample::select(invisible_chars()).prop_map(|c| c.to_string()),
            // The joiners alone: kept beside non-ASCII letters, hidden anywhere else.
            2 => Just(ZWNJ.to_string()),
            2 => Just(ZWJ.to_string()),
            3 => proptest::sample::select(letters),
            // A Persian word with its joiner inside, and a conjunct with its joiner.
            2 => Just(format!("{FA_WIKI}{ZWNJ}{FA_PEDIA}")),
            1 => Just(format!("{HI_KA}{ZWJ}{HI_SSA}")),
            // A word that ends in a virama and a joiner (the legacy chillu).
            2 => Just(format!("{ML_STEM}{ZWJ}")),
        ];
        let body = proptest::collection::vec(piece.clone(), 0..8).prop_map(|v| v.concat());
        let url = proptest::collection::vec(piece, 0..3)
            .prop_map(|v| format!("https://a.example/{}", v.concat()));
        let strategy = (
            body,
            proptest::option::weighted(0.8, url.clone()),
            proptest::collection::vec((0u32..40, 0u32..40, url), 0..3),
        );
        let hrefs_seen = std::cell::Cell::new(0usize);
        let joiner_kept = std::cell::Cell::new(0usize);
        let joiner_only_refused = std::cell::Cell::new(0usize);
        let joiner_last = std::cell::Cell::new(0usize);
        proptest!(|((body, whole, extra) in strategy)| {
            let mut ranges: Vec<(u32, u32, RangeKind)> = extra
                .into_iter()
                .map(|(start, len, url)| (start, len, RangeKind::Link(url)))
                .collect();
            if let Some(url) = whole {
                // An address that only the joiner rule refuses.
                if joiner_out_of_context(&url, &judged) && !set_in(&url) {
                    joiner_only_refused.set(joiner_only_refused.get() + 1);
                }
                ranges.push((0, body.len() as u32, RangeKind::Link(url)));
            }
            for r in [render(&text(&body, &ranges), &TestNames), render_plain(&body, &TestNames)] {
                assert_whitelisted(&r.html);
                let mut rest = r.html.as_str();
                while let Some(i) = rest.find("<a href=\"") {
                    let after = &rest[i + 9..];
                    let end = after.find('"').expect("a closed href");
                    let href = &after[..end];
                    hrefs_seen.set(hrefs_seen.get() + 1);
                    prop_assert!(
                        !hidden_in(href),
                        "an href holds one: {:?} of {:?}", href, body
                    );
                    if href.contains([ZWNJ, ZWJ]) {
                        joiner_kept.set(joiner_kept.get() + 1);
                    }
                    // A joiner that ends the address (it follows a virama; the judge above
                    // keeps it only so).
                    if href.ends_with([ZWNJ, ZWJ]) {
                        joiner_last.set(joiner_last.get() + 1);
                    }
                    let scheme = href.split(':').next().unwrap_or("").to_ascii_lowercase();
                    prop_assert!(ALLOWED_SCHEMES.contains(&scheme.as_str()), "a link of the scheme {}", scheme);
                    rest = &after[end + 2..];
                }
            }
        });
        eprintln!(
            "fuzz H1b: {} links, {} with a kept joiner, {} ending in one, {} addresses refused for a joiner alone",
            hrefs_seen.get(),
            joiner_kept.get(),
            joiner_last.get(),
            joiner_only_refused.get()
        );
        assert!(
            hrefs_seen.get() > 100,
            "the test made only {} links",
            hrefs_seen.get()
        );
        // (Over 20 runs: 35 to 70 links with a kept joiner, 22 to 37 addresses refused for a
        // joiner alone; FIX-2, over 12 runs: 48 to 78 with a kept joiner, 12 to 27 ending in
        // one, 14 to 32 refused for a joiner alone; the floors are a third of the lowest.)
        assert!(
            joiner_kept.get() > 12,
            "only {} links held a joiner",
            joiner_kept.get()
        );
        assert!(
            joiner_last.get() > 4,
            "only {} links ended in a joiner",
            joiner_last.get()
        );
        assert!(
            joiner_only_refused.get() > 8,
            "only {} addresses were refused for a joiner alone",
            joiner_only_refused.get()
        );
    }

    /// People pages P3: one mention per placeholder, the same as the menu's pages of people (both ask
    /// `Text::shown_mentions`): two persons on one placeholder draw and list the last, "everyone"
    /// last draws "@everyone" and lists nobody, a person last draws and lists the person.
    #[test]
    fn one_mention_stands_on_a_placeholder() {
        let m = MENTION_PLACEHOLDER;
        let body = format!("hi {m}");
        let range = |kind| (3, 3, kind);
        let olena = RangeKind::Mention(olena_id());
        let evil = RangeKind::Mention(UserId::parse("aci:evil").expect("id"));
        let both = render(
            &text(&body, &[range(olena.clone()), range(evil.clone())]),
            &TestNames,
        );
        assert_eq!(both.plain, "hi @<script>", "the last range is drawn");
        assert_eq!(both.mentions.len(), 1, "and it is the one listed");
        assert_eq!(both.mentions[0].as_str(), "aci:evil");
        assert_eq!(both.html.matches("kourier:mention/").count(), 1);
        assert_eq!(both.mention_spoilers.len(), 1);
        let all_last = render(
            &text(&body, &[range(olena.clone()), range(RangeKind::MentionAll)]),
            &TestNames,
        );
        assert_eq!(all_last.plain, "hi @everyone");
        assert!(all_last.mentions.is_empty(), "nobody is listed");
        assert!(!all_last.html.contains("kourier:"), "and no link is made");
        let person_last = render(
            &text(&body, &[range(RangeKind::MentionAll), range(olena)]),
            &TestNames,
        );
        assert_eq!(person_last.plain, "hi @Olena");
        assert_eq!(person_last.mentions, vec![olena_id()]);
        assert_eq!(person_last.html.matches("kourier:mention/0").count(), 1);
    }
}
