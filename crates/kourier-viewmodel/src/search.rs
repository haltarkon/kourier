//! The rows of a message search (G1g): the hits the core's `Query::Search` found, shown under
//! the chats of the list as chat rows (his answers "Under the chats", "Like a chat row").
//!
//! A row is a chat row with the hit in place of the preview: the chat's picture, name and
//! account chip, the time of the message, its sender ("You", a first name in a group, nobody in
//! a 1:1 chat or Note to Self: the list's own rule) and ONE line of the message, the snippet the
//! core cut ([`kourier_core::facade::Snippet`]).
//!
//! What a snippet may show (critic H1): its `text` is the raw cut, so it is rendered by the rule
//! of the list's previews ([`crate::preview::text_line`]): a spoiler's stretch is `■■■`, a mention
//! is `@Name`, and what matched is bold only where it touches no spoiler (the core already marks
//! none there; this side drops such a mark again: fail closed). The invisible and bidi controls go
//! by the rule of batch 1 ([`crate::richtext::hides_in_link`]): an override or a zero-width space
//! never reaches the label, a Persian or Hebrew word keeps its joiners. The line is HTML-escaped
//! for a `Text.StyledText` label, never `AutoText` (CLAUDE.md's text rule): `<b>` around the
//! matches is the only markup.
//!
//! Log safety (§9.1): rows hold names and message text; their `Debug` prints ids and flags only.

use std::time::Duration;

use kourier_core::facade::{SearchHit, Snippet};
use kourier_model::{
    AccountId, ConvKind, ConversationId, MessageId, NetworkKind, RangeKind,
    text::MENTION_PLACEHOLDER,
};

use crate::conversations::{AccountTable, ConversationRow};
use crate::i18n::trc;
use crate::names::{Names, first_name, single_line};
use crate::richtext::hides_in_link;
use crate::time::TimeContext;

/// Hits asked of the core per page (the first page, and each page asked on scroll).
pub const SEARCH_PAGE: u32 = 50;

/// How many capped pages in a row the search asks by itself while the list is shorter than a
/// page (privacy MEDIUM-2: a slow text, two letters on a big profile, would otherwise keep the
/// search connection busy with a chain of pages that each pay the whole expansion again). The
/// count starts again with a new search, a page he asks for and an answer that is not capped.
pub const MAX_CHAINED_PAGES: u32 = 8;

/// How long after a change of a chat of the shown hits they are read again (critic H5: a deleted
/// or expired message leaves the list "within a second"). The first change arms it, later ones
/// join it (a busy chat does not hold the read back for ever).
pub const REREAD_AFTER: Duration = Duration::from_millis(500);

/// How long after an answer whose index is NOT ready the hits are read again (G1i, decisions
/// call 3: the line "Older messages are still being prepared" must move by itself, and hits of
/// the newly indexed old messages must appear). One constant for the one re-read of a filling
/// index; an answer with a ready index arms nothing.
pub const INDEX_REREAD_AFTER: Duration = Duration::from_secs(2);

/// The longest wait between two polls of a filling index (F5, R3): the wait doubles after each
/// answer whose index is still filling, from [`INDEX_REREAD_AFTER`], and a new text starts at
/// the first again.
pub const INDEX_REREAD_MAX: Duration = Duration::from_secs(30);

/// The wait before the poll that follows `step` earlier polls of one text: `base`, doubled
/// `step` times, at most [`INDEX_REREAD_MAX`] (a `base` of zero, the tests', stays zero).
pub fn index_poll_delay(base: Duration, step: u32) -> Duration {
    let factor = 1u32.checked_shl(step.min(16)).unwrap_or(u32::MAX);
    base.saturating_mul(factor).min(INDEX_REREAD_MAX)
}

/// What stands for a spoiler's stretch (the chat list's rule, [`crate::preview::text_line`]).
pub const SPOILER_MASK: &str = "■■■";

/// The icon of a hit whose snippet is a file's name (the list's icon of a file).
pub const FILE_ICON: &str = "mail-attachment";

/// A snippet as one line: `html` for a `Text.StyledText` label (escaped, `<b>` around what
/// matched), `plain` the same text without markup (the accessible name, a copy, tests).
#[derive(Clone, Default, PartialEq, Eq, Hash)]
pub struct SnippetLine {
    /// Escaped text with `<b>…</b>` around the matches; no other markup.
    pub html: String,
    /// The displayed text.
    pub plain: String,
}

impl std::fmt::Debug for SnippetLine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SnippetLine")
            .field("bytes", &self.plain.len())
            .finish_non_exhaustive()
    }
}

/// One hit of a search, as a row under the chats.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct SearchRow {
    /// The message (the row's key).
    pub message: MessageId,
    /// Its chat.
    pub conv: ConversationId,
    /// The chat's account.
    pub account: AccountId,
    /// The account's network.
    pub network: NetworkKind,
    /// 1:1, group, …
    pub kind: ConvKind,
    /// Note to self.
    pub is_self: bool,
    /// The chat's name.
    pub title: String,
    /// The chat's picture, empty for initials.
    pub avatar_url: String,
    /// Who wrote the message: "You", the sender's first name in a group; empty in a 1:1 chat
    /// and in Note to Self (the list's rule for its preview's author).
    pub author: String,
    /// The line of the message that holds the hit.
    pub snippet: SnippetLine,
    /// An icon before the line ([`FILE_ICON`] when the line is a file's name), else empty.
    pub icon: String,
    /// The time of the message, as the list labels a time.
    pub time_label: String,
    /// The sender's timestamp (ms).
    pub sent_at: i64,
    /// The message's place in its timeline (the core's order of the hits).
    pub sort_key: i64,
    /// The chat is in the archive.
    pub archived: bool,
    /// The account's chip ("Work"), by the list's rule while searching.
    pub account_badge: String,
    /// The badge icon key of the account's network; empty when only one account exists.
    pub badge_icon: String,
    /// The account's number among the accounts of its network (0: none).
    pub badge_number: u32,
    /// The color of the chat's account (the chat row's wash while "tint rows" is on).
    pub tint: String,
    /// Who wrote the message, whole: "You" for an own message, else the sender's name (the
    /// partner's name in a 1:1 chat). The title of the row in a one-chat search (F4, R3).
    pub sender: String,
    /// The sender's picture: the account's own for an own message, the chat's picture in a 1:1
    /// chat, a group member's 1:1 chat picture (the timeline's rule; set by the source); empty
    /// otherwise (initials show).
    pub sender_avatar_url: String,
    /// The message is the account's own.
    pub own: bool,
}

crate::safe_debug!(SearchRow {
    message,
    conv,
    account,
    network,
    kind,
    is_self,
    sent_at,
    sort_key,
    archived,
    badge_number,
});

/// What a message search shows now ([`crate::UiSource::search_view`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchView {
    /// A message search runs (a text was asked about). `false`: show nothing of it.
    pub active: bool,
    /// The one chat it is limited to (`None`: every chat of every account).
    pub chat: Option<ConversationId>,
    /// The hits, newest first.
    pub rows: Vec<SearchRow>,
    /// More hits follow the last row ([`crate::UiSource::more_search`]).
    pub more: bool,
    /// An answer is on its way (the first page, a page more, a read again).
    pub loading: bool,
    /// Every message is indexed (`true` until an answer says otherwise).
    pub index_ready: bool,
    /// How much of the old messages is indexed (100 when ready).
    pub index_percent: u8,
    /// The last answer stopped early (its time, its size, a newer search, the index not judged
    /// yet): the rows may not be every hit, so an empty list is "still searching" or "type more
    /// letters", never "no results" (privacy MEDIUM-2).
    pub capped: bool,
    /// The core has answered THIS search (its text and chat) at least once: an empty list is
    /// then "No results" unless `capped`. `false` between the key and the first answer.
    pub answered: bool,
    /// The chat of a one-chat search is not in the list any more.
    pub chat_gone: bool,
    /// The core refused the pattern (too large to compile: the live check cannot know): the
    /// words, empty when it did not.
    pub refusal: String,
}

impl Default for SearchView {
    fn default() -> Self {
        Self {
            active: false,
            chat: None,
            rows: Vec::new(),
            more: false,
            loading: false,
            index_ready: true,
            index_percent: 100,
            capped: false,
            answered: false,
            chat_gone: false,
            refusal: String::new(),
        }
    }
}

/// The rows of `hits` (newest first) for one call: a hit shows only when `chat` gives the list
/// row of its chat (FAIL CLOSED: the caller gives none for a chat the list does not hold or calls
/// protected), each chat row decorated by `table` (made with `searching`), and every row names
/// its account when the rows are of several ([`AccountTable::finish`]).
pub fn hit_rows<'a>(
    hits: impl IntoIterator<Item = &'a SearchHit>,
    mut chat: impl FnMut(ConversationId) -> Option<ConversationRow>,
    table: &AccountTable,
    names: &dyn Names,
    time: &TimeContext<'_>,
) -> Vec<SearchRow> {
    let mut chats: std::collections::HashMap<ConversationId, Option<ConversationRow>> =
        std::collections::HashMap::new();
    let mut shown: Vec<&SearchHit> = Vec::new();
    let mut chat_rows: Vec<ConversationRow> = Vec::new();
    for hit in hits {
        let row = chats.entry(hit.conv).or_insert_with(|| {
            chat(hit.conv).map(|mut row| {
                table.decorate(&mut row);
                row
            })
        });
        if let Some(row) = row {
            shown.push(hit);
            chat_rows.push(row.clone());
        }
    }
    table.finish(&mut chat_rows);
    shown
        .into_iter()
        .zip(&chat_rows)
        .map(|(hit, chat)| search_row(hit, chat, names, time))
        .collect()
}

/// The row of `hit`, a message of the chat whose list row is `chat` (decorated by the account
/// table of the call, [`crate::conversations::AccountTable`]).
pub fn search_row(
    hit: &SearchHit,
    chat: &ConversationRow,
    names: &dyn Names,
    time: &TimeContext<'_>,
) -> SearchRow {
    let author = if chat.is_self {
        String::new()
    } else if hit.own {
        trc(
            "@item:intext author of the last message in the chat list, yourself",
            "You",
        )
        .into_owned()
    } else if chat.kind != ConvKind::Direct {
        hit.author_name
            .as_ref()
            .map(|n| first_name(&single_line(n.expose())).to_owned())
            .filter(|n| !n.is_empty())
            .or_else(|| {
                hit.author
                    .as_ref()
                    .map(|u| first_name(&names.display_name(u)).to_owned())
            })
            .unwrap_or_default()
    } else {
        String::new()
    };
    let you = || {
        trc(
            "@item:intext author of the last message in the chat list, yourself",
            "You",
        )
        .into_owned()
    };
    let sender = if hit.own {
        you()
    } else if chat.kind == ConvKind::Direct {
        chat.title.clone()
    } else {
        hit.author_name
            .as_ref()
            .map(|n| single_line(n.expose()))
            .filter(|n| !n.is_empty())
            .or_else(|| {
                hit.author
                    .as_ref()
                    .map(|u| single_line(&names.display_name(u)))
            })
            .unwrap_or_default()
    };
    SearchRow {
        message: hit.message,
        conv: hit.conv,
        account: hit.account,
        network: chat.network,
        kind: chat.kind,
        is_self: chat.is_self,
        title: chat.title.clone(),
        avatar_url: chat.avatar_url.clone(),
        author,
        snippet: snippet_line(&hit.snippet, names),
        icon: if hit.snippet.file.is_some() {
            FILE_ICON.to_owned()
        } else {
            String::new()
        },
        time_label: if hit.sent_at > 0 {
            time.list_label(hit.sent_at)
        } else {
            String::new()
        },
        sent_at: hit.sent_at,
        sort_key: hit.sort_key,
        archived: chat.archived,
        account_badge: chat.account_badge.clone(),
        badge_icon: chat.badge_icon.clone(),
        badge_number: chat.badge_number,
        tint: chat.tint.clone(),
        sender,
        sender_avatar_url: if !hit.own && chat.kind == ConvKind::Direct {
            chat.avatar_url.clone()
        } else {
            String::new()
        },
        own: hit.own,
    }
}

/// `snippet` as one line for a `Text.StyledText` label: a spoiler's stretch is [`SPOILER_MASK`], a
/// mention is `@Name`, a character that hides or reorders text ([`hides_in_link`]) and a
/// mention's placeholder without its range are left out, every other character is escaped, and
/// the matches are bold where no spoiler touches them.
///
/// Ranges that are not on character borders are taken as the core takes them: a spoiler is
/// WIDENED to the borders around it (it hides more, never less), a mention that is not exactly
/// its placeholder is no mention (its placeholder is left out), a mark that is not is dropped.
pub fn snippet_line(snippet: &Snippet, names: &dyn Names) -> SnippetLine {
    let text = snippet.text.as_str();
    let len = text.len();
    let floor = |mut at: usize| {
        at = at.min(len);
        while !text.is_char_boundary(at) {
            at -= 1;
        }
        at
    };
    let ceil = |mut at: usize| {
        at = at.min(len);
        while !text.is_char_boundary(at) {
            at += 1;
        }
        at
    };
    // The spoilers, widened to character borders, sorted and joined.
    let mut spoilers: Vec<(usize, usize)> = snippet
        .ranges
        .iter()
        .filter(|r| r.kind == RangeKind::Spoiler)
        .map(|r| {
            let start = r.start as usize;
            let end = start.saturating_add(r.len as usize);
            (floor(start), ceil(end))
        })
        .filter(|(s, e)| s < e)
        .collect();
    spoilers.sort_unstable();
    let mut joined: Vec<(usize, usize)> = Vec::with_capacity(spoilers.len());
    for (s, e) in spoilers {
        match joined.last_mut() {
            Some(last) if s <= last.1 => last.1 = last.1.max(e),
            _ => joined.push((s, e)),
        }
    }
    let spoilers = joined;
    let hidden = |s: usize, e: usize| spoilers.iter().any(|(a, b)| s <= *b && *a <= e);
    // The mentions: exactly a placeholder, outside every spoiler.
    let mut mentions: Vec<(usize, usize, &RangeKind)> = snippet
        .ranges
        .iter()
        .filter(|r| matches!(r.kind, RangeKind::Mention(_) | RangeKind::MentionAll))
        .filter_map(|r| {
            let start = r.start as usize;
            let end = start.checked_add(r.len as usize)?;
            let mut held = text.get(start..end)?.chars();
            (held.next() == Some(MENTION_PLACEHOLDER) && held.next().is_none())
                .then_some((start, end, &r.kind))
        })
        .filter(|(s, e, _)| !spoilers.iter().any(|(a, b)| s < b && a < e))
        .collect();
    mentions.sort_unstable_by_key(|(s, _, _)| *s);
    // The marks: on character borders, never touching a spoiler (fail closed, H1).
    let marks: Vec<(usize, usize)> = snippet
        .marks
        .iter()
        .filter_map(|m| {
            let start = m.start as usize;
            let end = start.checked_add(m.len as usize)?;
            text.get(start..end)?;
            (start < end && !hidden(start, end)).then_some((start, end))
        })
        .collect();
    let marked = |at: usize| marks.iter().any(|(s, e)| *s <= at && at < *e);

    let mut out = SnippetLine::default();
    let mut bold = false;
    let mut at = 0usize;
    while at < len {
        if let Some((_, end)) = spoilers.iter().find(|(s, e)| *s <= at && at < *e) {
            close_bold(&mut out.html, &mut bold);
            out.html.push_str(SPOILER_MASK);
            out.plain.push_str(SPOILER_MASK);
            at = *end;
            continue;
        }
        if let Some((_, end, kind)) = mentions.iter().find(|(s, _, _)| *s == at) {
            close_bold(&mut out.html, &mut bold);
            let name = match kind {
                RangeKind::Mention(user) => names.display_name(user),
                _ => "everyone".to_owned(),
            };
            let label = format!("@{}", clean(&single_line(&name)));
            escape_styled(&mut out.html, &label);
            out.plain.push_str(&label);
            at = *end;
            continue;
        }
        let Some(c) = text[at..].chars().next() else {
            break;
        };
        let next_at = at + c.len_utf8();
        let prev = text[..at].chars().next_back();
        let next = text[next_at..].chars().next();
        if c == MENTION_PLACEHOLDER || c.is_control() || hides_in_link(prev, c, next) {
            at = next_at;
            continue;
        }
        if marked(at) {
            if !bold {
                out.html.push_str("<b>");
                bold = true;
            }
        } else {
            close_bold(&mut out.html, &mut bold);
        }
        let mut buf = [0u8; 4];
        escape_styled(&mut out.html, c.encode_utf8(&mut buf));
        out.plain.push(c);
        at = next_at;
    }
    close_bold(&mut out.html, &mut bold);
    out
}

/// `s` without the characters that hide or reorder text ([`hides_in_link`], judged by their
/// neighbours in `s`).
fn clean(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    chars
        .iter()
        .enumerate()
        .filter(|(i, c)| {
            let prev = i.checked_sub(1).map(|p| chars[p]);
            let next = chars.get(i + 1).copied();
            !hides_in_link(prev, **c, next)
        })
        .map(|(_, c)| *c)
        .collect()
}

fn close_bold(html: &mut String, bold: &mut bool) {
    if std::mem::take(bold) {
        html.push_str("</b>");
    }
}

/// Escapes `s` for a `Text.StyledText` label: the three characters its parser reads as markup
/// (`&`, `<`, `>`) and the quote (`&quot;`, which it knows). An apostrophe stays: StyledText has
/// no `&#39;`.
fn escape_styled(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            c => out.push(c),
        }
    }
}

#[cfg(test)]
mod tests;
