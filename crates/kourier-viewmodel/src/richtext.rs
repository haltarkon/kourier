//! Message text → whitelisted Qt RichText (architecture §8.4, §9.4).
//!
//! The output uses only `<b> <i> <s> <code> <br/>` and `<a href>` with the schemes `http`,
//! `https` and `sgnl`; every text run is HTML-escaped, so markup in a message body is shown as
//! text and never reaches Qt as markup. Spoilers are not markup: their ranges are returned in
//! UTF-16 offsets of the *displayed* text (mentions expand to `@Name`, tabs to spaces), which
//! is what `TextEdit.positionToRectangle` takes, and QML draws opaque rectangles over them.
//!
//! Whitespace is preserved without CSS: runs of spaces become `&nbsp;` after the first and
//! newlines become `<br/>`, each counting as one displayed character like in Qt's document.

use std::collections::BTreeSet;

use kourier_model::text::{MENTION_PLACEHOLDER, validate_range};
use kourier_model::{RangeKind, Text};

use crate::names::Names;

/// URL schemes a link may use. Anything else renders as plain text.
pub const ALLOWED_SCHEMES: [&str; 3] = ["http", "https", "sgnl"];

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

/// Whether `url` may become a link: an allowed scheme, no whitespace or control characters,
/// and `//` plus a host for http(s). Returns the URL to put in `href` (unescaped).
pub fn allowed_href(url: &str) -> Option<&str> {
    if url.is_empty() || url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return None;
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
        let host = rest.strip_prefix("//")?;
        if host.is_empty() || host.starts_with('/') {
            return None;
        }
    } else if rest.is_empty() {
        return None;
    }
    Some(url)
}

/// Byte ranges of bare `http://` / `https://` URLs in `body`, as Signal Desktop linkifies them.
/// A URL starts at a word boundary and ends before whitespace, `<>"`, a control or mention
/// character; trailing punctuation and unbalanced closing brackets are not part of it.
pub fn find_urls(body: &str) -> Vec<(usize, usize)> {
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
        let mut end = pos + scheme_len;
        for (i, c) in body[end..].char_indices() {
            if c.is_whitespace()
                || c.is_control()
                || matches!(c, '<' | '>' | '"' | MENTION_PLACEHOLDER)
            {
                break;
            }
            end = pos + scheme_len + i + c.len_utf8();
        }
        end = trim_url_end(&body[pos..end]) + pos;
        if boundary && end > pos + scheme_len {
            out.push((pos, end));
            from = end;
        } else {
            from = pos + scheme_len;
        }
    }
    out
}

/// Length of `url` without trailing punctuation and unbalanced closing brackets.
fn trim_url_end(url: &str) -> usize {
    let mut end = url.len();
    loop {
        let Some(c) = url[..end].chars().next_back() else {
            return end;
        };
        let strip = match c {
            '.' | ',' | ':' | ';' | '!' | '?' | '\'' | '*' | '»' | '”' | '’' => true,
            ')' | ']' | '}' => {
                let open = match c {
                    ')' => '(',
                    ']' => '[',
                    _ => '{',
                };
                let s = &url[..end];
                s.matches(c).count() > s.matches(open).count()
            }
            _ => false,
        };
        if !strip {
            return end;
        }
        end -= c.len_utf8();
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
}

impl Emitter {
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

    // Bare URLs become links unless they touch an explicit link or code.
    let mut autolinks: Vec<(usize, usize, String)> = Vec::new();
    for (s, e) in find_urls(body) {
        let blocked = ranges.iter().any(|(rs, re, k)| {
            matches!(k, RangeKind::Link(_) | RangeKind::Mono) && *rs < e && s < *re
        });
        if !blocked {
            autolinks.push((s, e, body[s..e].to_owned()));
        }
    }

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
    };
    let mut open: Vec<Tag<'_>> = Vec::new();
    for w in cuts.windows(2) {
        let (a, b) = (w[0], w[1]);
        if a >= b {
            continue;
        }
        let mut style = Style::default();
        let mut link: Option<(usize, &str)> = None;
        let mut mention: Option<&RangeKind> = None;
        for (s, e, kind) in &ranges {
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
                RangeKind::Mention(_) | RangeKind::MentionAll => mention = Some(kind),
            }
        }
        if link.is_none() {
            link = autolinks
                .iter()
                .find(|(s, e, _)| *s <= a && b <= *e)
                .and_then(|(s, e, url)| allowed_href(url).map(|h| (e - s, h)));
        }

        let mut want: Vec<Tag<'_>> = Vec::with_capacity(5);
        if let Some((_, href)) = link {
            want.push(Tag::A(href));
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
            tag.open(&mut em.html);
            open.push(tag.clone());
        }

        let start16 = em.pos16;
        match mention {
            Some(kind) => {
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
        }
    }
    while let Some(tag) = open.pop() {
        tag.close(&mut em.html);
    }

    let jumbo = text.ranges.is_empty() && is_jumbo_emoji(body);
    RichText {
        html: em.html,
        plain: em.plain,
        spoilers: em.spoilers,
        jumbo,
        more: false,
        more_words: 0,
        whole: String::new(),
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
    use kourier_model::text::Utf16Map;
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
                    (p2, 3, RangeKind::Mention(evil)),
                ],
            ),
            &TestNames,
        );
        assert_eq!(r.html, "hi <b>@Olena</b> and <b>@&lt;script&gt;</b>!");
        assert_eq!(r.plain, "hi @Olena and @<script>!");
        // A placeholder without a mention range disappears.
        let r = render(&Text::plain(format!("a{MENTION_PLACEHOLDER}b")), &TestNames);
        assert_eq!(r.plain, "ab");
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
}
