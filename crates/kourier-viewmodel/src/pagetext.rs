//! The long plain texts of the details panel (people pages, F1): a group's description and a
//! person's about text, as the page shows them: selectable, with links in an accepted chat.
//!
//! The text is untrusted plain text. It is trimmed, cut at the page's limits ([`DESCRIPTION_LIMIT`]
//! graphemes, [`DESCRIPTION_BYTES`] bytes: a limit of the panel, not of the network; architecture
//! §13) and made into whitelisted HTML by [`richtext`] and nowhere else, the one detector the
//! timeline's messages use (a link is a bare `http://` or `https://` address, or an address
//! without a scheme by Signal Desktop's rule, which opens as `https://`). QML never builds an
//! `<a>` from the text.
//!
//! Links are the page's decision, not the text's: [`links_allowed`] says for which chats. Without
//! them the text is the same, escaped, and still selectable and copiable.

use kourier_model::{CannotSend, Text};
use unicode_segmentation::UnicodeSegmentation;

use crate::group::{DESCRIPTION_BYTES, DESCRIPTION_LIMIT};
use crate::names::NameMap;
use crate::richtext;

/// How many addresses of a text the page lists for the keyboard's menu ([`PageText::hrefs`]).
pub const HREFS_LISTED: usize = 5;

/// A long plain text as the page shows it.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct PageText {
    /// The shown text, line breaks kept (as `\n`), the ends trimmed, ending in "…" when it was cut.
    /// Empty when there is none.
    pub plain: String,
    /// The same text as whitelisted HTML for a read-only `TextEdit` (escaped, whitespace kept, a
    /// link around each address when links were asked for); empty when there is no text.
    pub html: String,
    /// The addresses the links of [`Self::html`] open, in the order of the text, each once, at
    /// most [`HREFS_LISTED`] (the keyboard's way to links is a menu with one entry for each).
    pub hrefs: Vec<String>,
}

/// Whether a chat in this state shows the links of a contact's text (a group's description, a
/// person's about text). The answer is a TABLE of the reasons, never "the chat can send":
///
/// | the chat's reason not to send | links |
/// |---|---|
/// | `RequestPending`, `NotAccepted`, `InvitationPending`, `JoinRequestPending`, `Blocked` | no (the chat is not accepted yet, or it is blocked) |
/// | any other reason (`AdminsOnly`, `NotYetBuilt`, `StillLoading`, `Unregistered`, `NotMember`, `Other`) | yes |
/// | none | yes |
///
/// An announcement group (`AdminsOnly`) is one he has accepted, and `NotYetBuilt` is every Threema
/// and Telegram chat until their send path exists: a test of `cannot_send.is_some()` would take the
/// links from all of them.
pub fn links_allowed(cannot_send: Option<&CannotSend>) -> bool {
    !matches!(
        cannot_send,
        Some(
            CannotSend::RequestPending
                | CannotSend::NotAccepted
                | CannotSend::InvitationPending
                | CannotSend::JoinRequestPending
                | CannotSend::Blocked
        )
    )
}

/// `text` as the page shows it, with links when `links` is true.
///
/// Line breaks are kept (`\r\n` and `\r` become `\n`), the ends trimmed, and the text is cut at the
/// first of [`DESCRIPTION_LIMIT`] graphemes or [`DESCRIPTION_BYTES`] bytes of UTF-8 (at the start
/// of a grapheme, except that one grapheme over the byte bound alone is cut at a char boundary),
/// then "…". The cut never falls inside an address: it moves back to the address's start, so a
/// link is never half an address (and the bounds still hold). Addresses are found in the part
/// that can show them only (the bound plus one more address), so a text of megabytes costs what
/// 16 KiB do.
pub fn page_text(text: Option<&str>, links: bool) -> PageText {
    let Some(text) = text.map(str::trim).filter(|t| !t.is_empty()) else {
        return PageText::default();
    };
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut cut = limit_cut(&text);
    // The addresses of the text, found on the part that reaches the cut and the longest address
    // beyond it (an address is at most 4,096 UTF-16 units, 12 KiB), never on a text of megabytes.
    let reach = cut.map_or(text.len(), |c| c.saturating_add(DESCRIPTION_BYTES));
    let reach = (0..=reach.min(text.len()))
        .rev()
        .find(|i| text.is_char_boundary(*i))
        .unwrap_or(0);
    let urls = richtext::find_urls(&text[..reach]);
    if let Some(at) = cut {
        // Out of an address: before its start, unless it starts the text (cannot be: an address
        // is shorter than both bounds), then the cut stays and the address is not a link.
        if let Some(&(start, _)) = urls.iter().find(|&&(s, e)| s < at && at < e)
            && start > 0
        {
            cut = Some(start);
        }
    }
    let shown = match cut {
        Some(at) => text[..at].trim_end(),
        None => text.as_str(),
    };
    // Whole addresses of the shown part only, found in the whole text (a part links what the
    // whole text links, as the timeline's cut text does).
    let urls: Vec<(usize, usize)> = if links {
        urls.into_iter()
            .filter(|&(_, e)| e <= shown.len())
            .collect()
    } else {
        Vec::new()
    };
    let mut hrefs: Vec<String> = Vec::new();
    for &(s, e) in &urls {
        let href = richtext::href_of_match(&shown[s..e]);
        if hrefs.len() < HREFS_LISTED && !hrefs.contains(&href) {
            hrefs.push(href);
        }
    }
    let mut rich = richtext::render_with(&Text::plain(shown), &NameMap::new(), urls);
    let mut plain = shown.to_owned();
    if cut.is_some() {
        plain.push('…');
        rich.html.push('…');
    }
    PageText {
        plain,
        html: rich.html,
        hrefs,
    }
}

/// Where the first of the page's two bounds cuts `text`, or `None` when all of it is shown.
fn limit_cut(text: &str) -> Option<usize> {
    for (n, (start, grapheme)) in text.grapheme_indices(true).enumerate() {
        if n == DESCRIPTION_LIMIT {
            return Some(start);
        }
        if start + grapheme.len() > DESCRIPTION_BYTES {
            return Some(if start > 0 {
                start
            } else {
                // One grapheme over the byte bound alone (a base letter with a million
                // combining marks): cut at a char boundary.
                (0..=DESCRIPTION_BYTES)
                    .rev()
                    .find(|i| text.is_char_boundary(*i))
                    .unwrap_or(0)
            });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every variant of [`CannotSend`] once. The match in [`links`] has no wildcard, so a new
    /// variant fails to build until someone decides whether its chat shows links.
    fn every_reason() -> Vec<CannotSend> {
        vec![
            CannotSend::NotMember,
            CannotSend::AdminsOnly,
            CannotSend::Blocked,
            CannotSend::RequestPending,
            CannotSend::Unregistered,
            CannotSend::NotAccepted,
            CannotSend::StillLoading,
            CannotSend::InvitationPending,
            CannotSend::JoinRequestPending,
            CannotSend::NotYetBuilt,
            CannotSend::Other("another reason".into()),
        ]
    }

    /// The table, written out: a chat that is not accepted yet (or is blocked) shows plain text.
    fn links(reason: &CannotSend) -> bool {
        match reason {
            CannotSend::RequestPending
            | CannotSend::NotAccepted
            | CannotSend::InvitationPending
            | CannotSend::JoinRequestPending
            | CannotSend::Blocked => false,
            CannotSend::NotMember
            | CannotSend::AdminsOnly
            | CannotSend::Unregistered
            | CannotSend::StillLoading
            | CannotSend::NotYetBuilt
            | CannotSend::Other(_) => true,
        }
    }

    /// One row per reason (critic M1). An announcement group (`AdminsOnly`) is accepted, and
    /// `NotYetBuilt` is every Threema and Telegram chat until its send path exists: the rule is the
    /// five reasons, never "the chat cannot send". Mutant: `cannot_send.is_none()`.
    #[test]
    fn links_follow_the_reason_of_an_unaccepted_chat_not_the_ability_to_send() {
        assert_eq!(every_reason().len(), 11, "a row for every variant");
        assert!(links_allowed(None), "a chat that can send");
        for reason in every_reason() {
            assert_eq!(
                links_allowed(Some(&reason)),
                links(&reason),
                "reason {reason:?}"
            );
        }
        assert!(links_allowed(Some(&CannotSend::NotYetBuilt)));
        assert!(links_allowed(Some(&CannotSend::AdminsOnly)));
        assert!(!links_allowed(Some(&CannotSend::Blocked)));
    }

    fn linked(text: &str) -> PageText {
        page_text(Some(text), true)
    }

    /// The addresses that become links on this path, and what stays plain: only what the one
    /// detector finds (critic M4). A scheme the detector never makes a link of, a bare `sgnl:`
    /// and the mention scheme stay text, so a description cannot open another program or somebody's
    /// page. (Widening `ALLOWED_SCHEMES` would change nothing here: a description has no link
    /// ranges, so only `find_urls` decides.)
    #[test]
    fn only_web_addresses_become_links_and_everything_else_stays_text() {
        let t = linked("Rules: https://example.org/a. Chat http://example.org:8080/b?c=1 now");
        assert_eq!(
            t.hrefs,
            ["https://example.org/a", "http://example.org:8080/b?c=1"]
        );
        assert!(
            t.html
                .contains("<a href=\"https://example.org/a\">https://example.org/a</a>.")
        );
        // A bare domain is a link that opens as https (LD).
        let t = linked("see www.example.org/x and example.net");
        assert_eq!(
            t.hrefs,
            ["https://www.example.org/x", "https://example.net"]
        );
        for plain in [
            "ftp://example.org/a",
            "javascript:alert(1)",
            "file:///etc/passwd",
            "mailto:someone@example.org",
            "tel:+10000000000",
            "sgnl://signal.me/#p/x",
            "sgnl:",
            "kourier:mention/0",
            "see kourier:mention/0 and @name",
            "data:text/html;base64,AAAA",
        ] {
            let t = linked(plain);
            assert!(!t.html.contains("<a "), "a link in {plain:?}: {}", t.html);
            assert!(t.hrefs.is_empty(), "{plain:?}");
            assert_eq!(t.plain, plain);
        }
    }

    /// Markup in the text is shown as text and never reaches Qt as markup, with links on or off
    /// (critic M4). Mutant: skip the escaping.
    #[test]
    fn markup_in_the_text_is_escaped() {
        let body =
            "<img src=\"x\" onerror=\"y\"> & <b>bold</b> https://example.org/?a=1&b=2 <script>";
        for on in [true, false] {
            let t = page_text(Some(body), on);
            assert!(!t.html.contains("<img"), "{}", t.html);
            assert!(!t.html.contains("<b>"), "{}", t.html);
            assert!(!t.html.contains("<script"), "{}", t.html);
            assert!(t.html.contains("&lt;img src=&quot;x&quot;"), "{}", t.html);
            assert!(t.html.contains("&amp;"), "{}", t.html);
            assert_eq!(t.plain, body, "the plain text is the text as written");
        }
    }

    /// Without links the text is the same, escaped, with no `<a` and no address listed.
    #[test]
    fn without_links_the_text_is_the_same_and_plain() {
        let body = "Join https://example.org/a or example.net\nNext line";
        let on = page_text(Some(body), true);
        let off = page_text(Some(body), false);
        assert_eq!(on.plain, off.plain);
        assert!(on.html.contains("<a href="));
        assert!(!off.html.contains("<a "), "{}", off.html);
        assert!(off.hrefs.is_empty());
        // (The line break and the address text are in both.)
        assert_eq!(
            on.html
                .replace("<a href=\"https://example.org/a\">", "")
                .replace("<a href=\"https://example.net\">", "")
                .replace("</a>", ""),
            off.html
        );
        assert!(off.html.contains("<br/>"));
    }

    /// An invisible or reordering character ends the address before it, so the link's text, its
    /// `href` and the question's address cannot read as another address (H1's table, through this
    /// path). The characters are escapes: a tool that edits this file can turn a literal one into
    /// the invisible character.
    #[test]
    fn an_address_ends_before_an_invisible_or_bidi_character() {
        for hidden in [
            '\u{202e}', '\u{200b}', '\u{2066}', '\u{2069}', '\u{061c}', '\u{2060}',
        ] {
            let t = linked(&format!("https://a.example/path{hidden}moc.evil/x"));
            assert_eq!(t.hrefs, ["https://a.example/path"], "{:04x}", hidden as u32);
            assert!(
                t.hrefs
                    .iter()
                    .all(|h| !h.chars().any(richtext::is_invisible_or_bidi)),
                "{:04x}",
                hidden as u32
            );
        }
        // A character in the middle of a host splits it: no link at all for the part after it.
        let t = linked("https://exam\u{200b}ple.org/");
        assert!(
            t.hrefs.iter().all(|h| !h.contains('\u{200b}')),
            "{:?}",
            t.hrefs
        );
    }

    /// A long text is cut at 4,096 characters (or 16 KiB) and the cut never falls inside an address:
    /// it moves back to the address's start, so a link is never half an address (critic M4, F1
    /// trap 8). Mutant: cut at the plain index.
    #[test]
    fn the_cut_never_falls_inside_an_address() {
        let lead = "x".repeat(DESCRIPTION_LIMIT - 10);
        let url = "https://example.org/a-very-long-path-of-the-address";
        // The address starts 9 characters before the limit and goes on past it.
        let body = format!("{lead} {url} tail");
        let t = linked(&body);
        assert_eq!(t.plain, format!("{lead}…"), "the address is left out whole");
        assert!(t.hrefs.is_empty() && !t.html.contains("<a "), "{}", t.html);
        assert!(!t.plain.contains("https"));
        // An address that ends before the limit stays, whole and linked.
        let lead = "x".repeat(DESCRIPTION_LIMIT - url.len() - 12);
        let t = linked(&format!("{lead} {url} {}", "y".repeat(100)));
        assert_eq!(t.hrefs, [url]);
        assert!(t.html.contains(&format!("<a href=\"{url}\">{url}</a>")));
        assert!(t.plain.ends_with('…'));
        // One that ends exactly at the limit is whole too.
        let lead = "x".repeat(DESCRIPTION_LIMIT - url.len() - 1);
        let t = linked(&format!("{lead} {url} more"));
        assert_eq!(t.hrefs, [url], "{}", t.plain);
        assert!(t.plain.ends_with("…") && t.plain.contains(url));
        // The same with the links off: the text does not change when a request is accepted.
        let body = format!("{} {url} tail", "x".repeat(DESCRIPTION_LIMIT - 10));
        assert_eq!(
            page_text(Some(&body), false).plain,
            page_text(Some(&body), true).plain
        );
        // The byte bound: families of emoji (18 bytes, ONE character each) reach 16 KiB long before 4,096
        // characters, and the cut at the bound ends inside an address of 220 characters.
        let family = "👨\u{200d}👩\u{200d}👧";
        let long = format!("https://example.org/{}", "p".repeat(200));
        let body = format!("{} {long} tail", family.repeat(905));
        let t = linked(&body);
        assert_eq!(
            t.plain,
            format!("{}…", family.repeat(905)),
            "the address is left out whole"
        );
        assert!(t.plain.len() <= DESCRIPTION_BYTES + '…'.len_utf8());
        assert!(t.hrefs.is_empty() && !t.html.contains("<a "));
    }

    /// A bare domain cut after its top-level domain is not a link when the whole text does not
    /// link it (`foo.debar` is no address, `foo.de` of its first characters would be).
    #[test]
    fn a_part_links_what_the_whole_text_links() {
        let lead = "x".repeat(DESCRIPTION_LIMIT - 7);
        let t = linked(&format!("{lead} foo.debar baz"));
        assert!(
            t.plain.ends_with(" foo.de…"),
            "{}",
            &t.plain[t.plain.len() - 12..]
        );
        assert!(!t.html.contains("<a "), "{}", t.html);
        assert!(t.hrefs.is_empty());
        // The control: the whole text of `foo.de` does link it.
        let t = linked(&format!("{lead} foo.de baz"));
        assert_eq!(t.hrefs, ["https://foo.de"]);
    }

    /// The ellipsis is no part of an address: an address that ends where the text is cut is linked
    /// without it, and one that runs on past the cut is left out.
    #[test]
    fn the_ellipsis_is_outside_the_link() {
        let url = "https://example.org/a";
        let lead = "x".repeat(DESCRIPTION_LIMIT - url.len() - 1);
        let t = linked(&format!("{lead} {url}z more"));
        assert!(t.hrefs.is_empty(), "{:?}", t.hrefs);
        let t = linked(&format!("{lead} {url} more"));
        assert!(
            t.html.ends_with("</a>…"),
            "{}",
            &t.html[t.html.len() - 20..]
        );
    }

    /// Only the first [`HREFS_LISTED`] different addresses are listed (the keyboard's menu), each
    /// once, in the order of the text.
    #[test]
    fn the_listed_addresses_are_few_and_distinct() {
        let body = (0..9)
            .map(|n| format!("https://example.org/{}", n % 7))
            .collect::<Vec<_>>()
            .join(" ");
        let t = linked(&body);
        assert_eq!(t.hrefs.len(), HREFS_LISTED);
        assert_eq!(t.hrefs[0], "https://example.org/0");
        assert_eq!(t.hrefs[4], "https://example.org/4");
        // Every address of the text is still a link in the HTML.
        assert_eq!(t.html.matches("<a href=").count(), 9);
    }

    /// Text of megabytes costs what 16 KiB do, and a hostile run of closing brackets after an address
    /// does not make the detector slow (it was quadratic in the run's length).
    #[test]
    fn a_hostile_text_is_cheap() {
        let started = std::time::Instant::now();
        for filler in [")", "]", "}", ".", "a", "é"] {
            let body = format!("https://a.example/{}", filler.repeat(3_000_000));
            let t = linked(&body);
            assert!(t.plain.len() <= DESCRIPTION_BYTES + '…'.len_utf8());
        }
        let body = format!("{}{}", "(".repeat(20_000), ")".repeat(20_000));
        let _ = linked(&format!("see https://a.example/{body}"));
        assert!(
            started.elapsed() < std::time::Duration::from_secs(20),
            "slow: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn nothing_and_blanks_are_no_text() {
        for none in [None, Some(""), Some("  \n \r\n ")] {
            assert!(page_text(none, true) == PageText::default());
        }
        let t = page_text(Some("  a\r\nb\rc  "), true);
        assert_eq!(t.plain, "a\nb\nc");
        assert_eq!(t.html, "a<br/>b<br/>c");
    }
}
