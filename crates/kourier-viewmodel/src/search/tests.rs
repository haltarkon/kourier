//! The rows of a search (G1g): the snippet's line (escaped, spoilers masked, mentions named, the
//! bidi rule of batch 1, marks never in a spoiler) and the row of a hit (a chat row).

use kourier_core::facade::{SearchHit, Snippet, SnippetMark};
use kourier_model::{
    AccountId, ConvKind, ConversationId, MessageId, NetworkKind, RangeKind, Redacted, TextRange,
    UserId,
};

use super::*;
use crate::conversations::{AccountTable, Hidden, PreviewSource, TableAccount};
use crate::names::NameMap;
use crate::time::FixedOffset;
use crate::timeline::Receipt;

const NOW: i64 = 1_760_000_000_000;

fn user(n: u32) -> UserId {
    UserId::parse(&format!("aci:00000000-0000-4000-8000-{n:012x}")).expect("id")
}

/// A snippet of `text` with a mark on each of `marked` (its first place) and `ranges`.
fn snippet(text: &str, marked: &[&str], ranges: Vec<TextRange>) -> Snippet {
    Snippet {
        text: text.to_owned(),
        marks: marked
            .iter()
            .map(|m| {
                let start = text.find(m).expect("marked word in the text");
                SnippetMark {
                    start: start as u32,
                    len: m.len() as u32,
                }
            })
            .collect(),
        ranges,
        file: None,
    }
}

fn range(text: &str, of: &str, kind: RangeKind) -> TextRange {
    let start = text.find(of).expect("range in the text");
    TextRange {
        start: start as u32,
        len: of.len() as u32,
        kind,
    }
}

fn line(s: &Snippet) -> SnippetLine {
    snippet_line(s, &NameMap::new())
}

/// Markup in a message is shown as text: nothing of it reaches the label as markup, and the
/// only tags are the bold ones around what matched.
#[test]
fn a_search_snippet_escapes_markup_and_bolds_only_what_matched() {
    let text = "<img src=x> & \"Файли\" <b>x</b> it's";
    let l = line(&snippet(text, &["Файли"], Vec::new()));
    assert_eq!(
        l.html,
        "&lt;img src=x&gt; &amp; &quot;<b>Файли</b>&quot; &lt;b&gt;x&lt;/b&gt; it's"
    );
    assert_eq!(l.plain, text);
    // Whatever a text holds, the markup has no tag but `<b>` and `</b>`.
    let hostile = "</b><i>a</i><a href=\"https://e.org\">l</a><br/>&lt;";
    let l = line(&snippet(hostile, &["a"], Vec::new()));
    let tags: Vec<&str> = l
        .html
        .match_indices('<')
        .map(|(i, _)| &l.html[i..])
        .collect();
    assert!(
        tags.iter()
            .all(|t| t.starts_with("<b>") || t.starts_with("</b>")),
        "only bold tags: {:?}",
        l.html
    );
    assert!(!l.html.contains("<i>") && !l.html.contains("<a ") && !l.html.contains("<br"));
    assert!(
        l.html.contains("&amp;lt;"),
        "an entity in the text stays text"
    );
}

/// H1: a spoiler's stretch is `■■■` and never its text; a mark that touches it is not drawn
/// (the core marks none there; this side drops one again); the marks outside it are.
#[test]
fn a_search_snippet_masks_a_spoiler_and_draws_no_mark_in_it() {
    let text = "the secret word is plan, the rest";
    let s = snippet(
        text,
        &["secret", "plan"],
        vec![range(text, "secret word", RangeKind::Spoiler)],
    );
    let l = line(&s);
    assert_eq!(l.plain, "the ■■■ is plan, the rest");
    assert_eq!(l.html, "the ■■■ is <b>plan</b>, the rest");
    assert!(!l.html.contains("secret") && !l.html.contains("word"));
    // A mark right beside the spoiler (touching it) is not drawn either.
    let text = "abc plan";
    let s = snippet(
        text,
        &["abc"],
        vec![range(text, " plan", RangeKind::Spoiler)],
    );
    assert_eq!(line(&s).html, "abc■■■");
    // Two spoilers that overlap or touch are one mask.
    let text = "one two three four";
    let s = snippet(
        text,
        &["four"],
        vec![
            range(text, "one two", RangeKind::Spoiler),
            range(text, "two three", RangeKind::Spoiler),
        ],
    );
    assert_eq!(line(&s).plain, "■■■ four");
}

/// A spoiler range that is not on character borders is WIDENED (it hides more, never less), one
/// past the end stops at the end; a mark that is not on borders is dropped; a mention that is not
/// exactly its placeholder is no mention.
#[test]
fn a_search_snippet_widens_a_spoiler_off_the_character_borders() {
    let text = "ключ і план";
    // "ключ" is 8 bytes: a spoiler from byte 1 to 7 cuts its first and last letter; one from
    // byte 13 (inside the "п" of "план") runs past the end.
    let s = Snippet {
        text: text.to_owned(),
        marks: vec![SnippetMark { start: 1, len: 3 }],
        ranges: vec![
            TextRange {
                start: 1,
                len: 6,
                kind: RangeKind::Spoiler,
            },
            TextRange {
                start: 13,
                len: 99,
                kind: RangeKind::Spoiler,
            },
        ],
        file: None,
    };
    let l = line(&s);
    assert_eq!(l.plain, "■■■ і ■■■");
    assert!(!l.plain.contains('к') && !l.plain.contains('ч') && !l.plain.contains('н'));
    assert!(!l.html.contains("<b>"));
}

/// H1: a mention shows as `@Name` (the name the source knows, else the fallback), "everyone" for
/// a mention of everyone; a placeholder without its range is left out.
#[test]
fn a_search_snippet_names_a_mention_and_drops_a_bare_placeholder() {
    let mut names = NameMap::new();
    names.insert(user(7), "Olena\nKovalenko");
    let text = "hi \u{FFFC} and \u{FFFC}, see the plan \u{FFFC}";
    let first = 3;
    let second = text[first + 3..].find('\u{FFFC}').expect("second") + first + 3;
    let s = Snippet {
        text: text.to_owned(),
        marks: vec![SnippetMark {
            start: text.find("plan").expect("plan") as u32,
            len: 4,
        }],
        ranges: vec![
            TextRange {
                start: first as u32,
                len: 3,
                kind: RangeKind::Mention(user(7)),
            },
            TextRange {
                start: second as u32,
                len: 3,
                kind: RangeKind::MentionAll,
            },
        ],
        file: None,
    };
    let l = snippet_line(&s, &names);
    assert_eq!(l.plain, "hi @Olena Kovalenko and @everyone, see the plan ");
    assert_eq!(
        l.html,
        "hi @Olena Kovalenko and @everyone, see the <b>plan</b> "
    );
    assert!(!l.html.contains('\u{FFFC}'));
    // A mention's name with markup is escaped too.
    names.insert(user(7), "<b>Ari</b>");
    let l = snippet_line(&s, &names);
    assert!(l.html.starts_with("hi @&lt;b&gt;Ari&lt;/b&gt; and"));
}

/// The bidi rule of batch 1 (`hides_in_link`): an override, an embedding, an isolate, a mark and
/// a zero-width space never reach the label; a Persian word keeps its zero-width non-joiner and
/// an emoji sequence its joiner; a joiner between ASCII letters goes. A mark after a dropped
/// character still covers its word.
#[test]
fn a_search_snippet_drops_bidi_overrides_and_keeps_joiners() {
    let persian = "می\u{200C}خواهم";
    let family = "👩\u{200D}👧";
    let text = format!(
        "a\u{202E}gpj.exe\u{202C} b\u{200B}c {persian} ex\u{200D}ample \u{2066}x\u{2069} {family} \u{200F}plan"
    );
    let l = line(&snippet(&text, &["plan"], Vec::new()));
    for gone in [
        '\u{202E}', '\u{202C}', '\u{200B}', '\u{2066}', '\u{2069}', '\u{200F}',
    ] {
        assert!(!l.html.contains(gone), "{:04X} stays out", u32::from(gone));
        assert!(!l.plain.contains(gone));
    }
    assert!(l.plain.contains(persian), "the Persian word keeps its ZWNJ");
    assert!(l.plain.contains(family), "the emoji sequence keeps its ZWJ");
    assert!(
        l.plain.contains("example"),
        "a joiner between ASCII letters goes"
    );
    assert!(l.html.ends_with(" <b>plan</b>"), "{:?}", l.html);
}

fn chat(kind: ConvKind) -> ConversationRow {
    ConversationRow {
        id: ConversationId(4),
        account: AccountId(1),
        network: NetworkKind::Signal,
        kind,
        is_self: false,
        title: "Drone club".into(),
        avatar_url: "file:///avatar.png".into(),
        preview_author: "Ari".into(),
        preview_source: PreviewSource::Peer,
        preview: "the chat's own preview".into(),
        preview_icon: String::new(),
        typing: String::new(),
        time_label: String::new(),
        last_activity: NOW,
        unread: 3,
        mentions: 0,
        marked_unread: false,
        muted: false,
        pinned_order: None,
        archived: true,
        receipt: Receipt::None,
        account_badge: "Work".into(),
        badge_icon: "signal".into(),
        tint: String::new(),
        badge_number: 2,
        account_order: 0,
        order_at: 0,
        protected: false,
        title_marks: Vec::new(),
    }
}

fn hit(conv: i64, n: i64, own: bool) -> SearchHit {
    SearchHit {
        message: MessageId(conv * 1000 + n),
        conv: ConversationId(conv),
        account: AccountId(1),
        sort_key: n,
        sent_at: NOW - 60_000,
        own,
        author: (!own).then(|| user(3)),
        author_name: (!own).then(|| Redacted::new("Ari\tArden".to_owned())),
        snippet: snippet("bring the plan", &["plan"], Vec::new()),
    }
}

/// His answer "Like a chat row": the chat's name, picture, chip and archive flag, the time of the
/// message, its line, and the sender by the list's rule ("You", a first name in a group, nobody
/// in a 1:1 chat or Note to Self); a file's name gets the file icon.
#[test]
fn a_search_row_is_a_chat_row_with_the_hit_in_place_of_the_preview() {
    let cal = FixedOffset(0);
    let time = TimeContext::new(NOW, &cal);
    let names = NameMap::new();
    let group = chat(ConvKind::Group);
    let r = search_row(&hit(4, 1, false), &group, &names, &time);
    assert_eq!(r.title, "Drone club");
    assert_eq!(r.avatar_url, "file:///avatar.png");
    assert_eq!(r.author, "Ari");
    assert_eq!(r.snippet.html, "bring the <b>plan</b>");
    assert_eq!(r.time_label, time.list_label(NOW - 60_000));
    assert!(r.archived);
    assert_eq!(
        (
            r.account_badge.as_str(),
            r.badge_icon.as_str(),
            r.badge_number
        ),
        ("Work", "signal", 2)
    );
    assert!(r.icon.is_empty());
    assert_eq!(
        search_row(&hit(4, 1, true), &group, &names, &time).author,
        "You"
    );
    let direct = chat(ConvKind::Direct);
    assert_eq!(
        search_row(&hit(4, 1, false), &direct, &names, &time).author,
        ""
    );
    assert_eq!(
        search_row(&hit(4, 1, true), &direct, &names, &time).author,
        "You"
    );
    let me = ConversationRow {
        is_self: true,
        ..chat(ConvKind::Direct)
    };
    assert_eq!(search_row(&hit(4, 1, true), &me, &names, &time).author, "");
    // A sender with no name from the core: the name the source knows.
    let mut known = NameMap::new();
    known.insert(user(3), "Maksym Bondar");
    let mut nameless = hit(4, 1, false);
    nameless.author_name = None;
    assert_eq!(
        search_row(&nameless, &group, &known, &time).author,
        "Maksym"
    );
    // A file's name.
    let mut file = hit(4, 1, false);
    file.snippet.file = Some(0);
    assert_eq!(search_row(&file, &group, &names, &time).icon, FILE_ICON);
}

/// FAIL CLOSED: a hit whose chat the caller gives no row for shows nothing; the rows of two
/// accounts both name their account (the list's rule while searching).
#[test]
fn search_hit_rows_leave_out_a_chat_without_a_row_and_name_both_accounts() {
    let cal = FixedOffset(0);
    let time = TimeContext::new(NOW, &cal);
    let names = NameMap::new();
    let table = AccountTable::new(
        [
            TableAccount {
                id: AccountId(1),
                network: NetworkKind::Signal,
                name: "Home".into(),
                tint: String::new(),
            },
            TableAccount {
                id: AccountId(2),
                network: NetworkKind::Signal,
                name: "Work".into(),
                tint: String::new(),
            },
        ],
        &Hidden::Ids([AccountId(2)].into()),
        true,
    );
    let mut other = hit(5, 2, false);
    other.account = AccountId(2);
    let hits = [hit(4, 1, false), hit(9, 1, false), other];
    let rows = hit_rows(
        &hits,
        |conv| match conv.0 {
            4 => Some(ConversationRow {
                account_badge: String::new(),
                badge_number: 0,
                ..chat(ConvKind::Group)
            }),
            5 => Some(ConversationRow {
                id: ConversationId(5),
                account: AccountId(2),
                account_badge: String::new(),
                badge_number: 0,
                ..chat(ConvKind::Group)
            }),
            _ => None,
        },
        &table,
        &names,
        &time,
    );
    assert_eq!(
        rows.iter().map(|r| r.conv.0).collect::<Vec<_>>(),
        vec![4, 5],
        "the hit of chat 9 has no chat row"
    );
    assert_eq!(
        rows.iter()
            .map(|r| r.account_badge.as_str())
            .collect::<Vec<_>>(),
        vec!["Home", "Work"]
    );
}

/// §9.1: a row's and a line's `Debug` print no text, no name.
#[test]
fn search_rows_print_no_text_in_debug() {
    let cal = FixedOffset(0);
    let time = TimeContext::new(NOW, &cal);
    let r = search_row(
        &hit(4, 1, false),
        &chat(ConvKind::Group),
        &NameMap::new(),
        &time,
    );
    let view = SearchView {
        active: true,
        rows: vec![r.clone()],
        ..SearchView::default()
    };
    for printed in [
        format!("{r:?}"),
        format!("{view:?}"),
        format!("{:?}", r.snippet),
    ] {
        for secret in ["plan", "Drone", "Ari", "Work", "avatar"] {
            assert!(!printed.contains(secret), "{secret} in {printed}");
        }
    }
}
