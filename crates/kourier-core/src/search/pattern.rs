//! Pattern search (R5, architecture §8.4): [`SearchMode::Pattern`](crate::facade::SearchMode).
//!
//! The text of the query is a regular expression in the filters' syntax and limits
//! ([`filters::compile`]: Unicode, multi-line, 256 KiB compiled, case ignored unless asked). It
//! is compiled HERE, on the search's blocking thread, never on the Qt thread; a pattern the
//! compile refuses is a typed [`FilterRefusal`] (`CoreError::Filter`), never a log line and never
//! a `regex::Error` (its Display quotes the pattern).
//!
//! THE SCAN steps `messages` itself, newest first, under the SAME query-time rules as the word
//! search ([`hit_rules`]: not deleted, not view-once, not hidden, the chat not deleted and not
//! protected, the account not being removed), decodes each body, and runs the regex over
//! [`searched_texts`](super::normalize::searched_texts) ([`pattern_snippet`]). Two orders, both
//! keyset cursors over the last row LOOKED AT (hit or not), so every page goes on below where
//! the one before stopped:
//! - a search of one chat (the per-chat chip) walks `messages_timeline`, `(sort_key, id)`;
//! - every other search walks the primary key, `id` newest first (arrival order, which is the
//!   time order but for imported history); `SearchCursor::sort_key` is not read.
//!
//! A page ends at `limit` hits, at the end of the table (`next: None`), or at the deadline
//! (`capped`, with a `next`); the clock is read every [`CLOCK_EVERY`] rows, and never before the
//! first row, so a page always makes progress. A newer search interrupts the statement: the page
//! answers what it found, `capped`. Nothing here logs; the pattern is never in an error.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;

use kourier_db::rusqlite::Connection;
use kourier_model::{Body, MessageId};

use crate::error::{CoreError, Result};
use crate::facade::{
    MAX_SEARCH_PAGE, SearchCursor, SearchIndexState, SearchPage, SearchPattern, SearchQuery,
};
use crate::filters::{self, Field, FilterRefusal, MAX_PATTERN_CHARS};
use crate::sql;

use super::query::{Found, Walked, again, hit, hit_rules, interrupted};
use super::snippet::pattern_snippet;

/// Rows between two looks at the clock.
const CLOCK_EVERY: usize = 256;

/// Every chat: the primary key, newest first (`?1` = the id the page goes on below).
pub(crate) const ID_SCAN: &str = concat!(
    "SELECT m.id, m.conversation_id, c.account_id, m.sort_key, m.sent_at, m.net_author, m.body
     FROM messages m
     CROSS JOIN conversations c ON c.id = m.conversation_id
     CROSS JOIN accounts a ON a.id = c.account_id
     WHERE m.id < ?1 AND ",
    hit_rules!(),
    "
     ORDER BY m.id DESC"
);

/// One chat: `messages_timeline` (`?1` = the chat, `?2`/`?3` = the keyset the page goes on below).
pub(crate) const TIMELINE_SCAN: &str = concat!(
    "SELECT m.id, m.conversation_id, c.account_id, m.sort_key, m.sent_at, m.net_author, m.body
     FROM messages m
     CROSS JOIN conversations c ON c.id = m.conversation_id
     CROSS JOIN accounts a ON a.id = c.account_id
     WHERE m.conversation_id = ?1 AND (m.sort_key, m.id) < (?2, ?3) AND ",
    hit_rules!(),
    "
     ORDER BY m.sort_key DESC, m.id DESC"
);

/// One page of the pattern search `q` on `core`.
pub(crate) fn scan(
    core: &Connection,
    q: &SearchQuery,
    match_case: bool,
    index: SearchIndexState,
    deadline: Instant,
) -> Result<Walked> {
    let limit = usize::try_from(q.limit.clamp(1, MAX_SEARCH_PAGE)).unwrap_or(1);
    let text = q.text.expose();
    let page = |hits, next, capped, pattern| SearchPage {
        hits,
        next,
        capped,
        index,
        pattern,
    };
    // No pattern, or a grant of no chat, answers an empty page: never every chat (M11).
    if text.is_empty() || q.chats.as_ref().is_some_and(Vec::is_empty) {
        return Ok(Walked {
            page: page(Vec::new(), None, false, None),
            looked: 0,
        });
    }
    if text.chars().count() > MAX_PATTERN_CHARS {
        return Err(CoreError::Filter(FilterRefusal::PatternTooLong(
            Field::Text,
        )));
    }
    let re = filters::compile(text, match_case)
        .map_err(|e| CoreError::Filter(FilterRefusal::Pattern(Field::Text, e)))?;
    let scope: Option<HashSet<i64>> = q.chats.as_ref().map(|c| c.iter().map(|c| c.0).collect());
    let one_chat = q.chats.as_ref().filter(|c| c.len() == 1).map(|c| c[0].0);

    let mut stmt = core.prepare_cached(if one_chat.is_some() {
        TIMELINE_SCAN
    } else {
        ID_SCAN
    })?;
    let bound = q
        .before
        .map_or((i64::MAX, i64::MAX), |c| (c.sort_key, c.id.0));
    let mut rows = match one_chat {
        Some(conv) => stmt.query((conv, bound.0, bound.1))?,
        None => stmt.query([q.before.map_or(i64::MAX, |c| c.id.0)])?,
    };

    let mut found: Vec<Found> = Vec::new();
    let mut scanned = 0_usize;
    let mut last: Option<(i64, i64)> = None;
    let (mut capped, mut exhausted) = (false, false);
    loop {
        if scanned > 0 && scanned.is_multiple_of(CLOCK_EVERY) && Instant::now() >= deadline {
            capped = true;
            break;
        }
        let row = match rows.next() {
            Ok(Some(row)) => row,
            Ok(None) => {
                exhausted = true;
                break;
            }
            Err(e) if interrupted(&e) => {
                capped = true;
                break;
            }
            Err(e) => return Err(e.into()),
        };
        let id: i64 = row.get(0)?;
        let conv: i64 = row.get(1)?;
        let account: i64 = row.get(2)?;
        let sort_key: i64 = row.get(3)?;
        let sent_at: i64 = row.get(4)?;
        let author: String = row.get(5)?;
        let body: String = row.get(6)?;
        scanned += 1;
        last = Some((sort_key, id));
        if scope.as_ref().is_some_and(|s| !s.contains(&conv)) {
            continue;
        }
        // A body that does not parse is not searchable (M9).
        let Ok(body) = sql::from_json::<Body>(&body) else {
            continue;
        };
        let Some(snippet) = pattern_snippet(&body, &re) else {
            continue;
        };
        found.push(Found {
            rowid: id,
            id,
            conv,
            account,
            sort_key,
            sent_at,
            author,
            snippet,
        });
        if found.len() >= limit {
            break;
        }
    }
    drop(rows);
    drop(stmt);
    let next = if exhausted {
        None
    } else {
        // Below the last row looked at; a page that looked at nothing asks its own page again
        // (never a final "nothing" for a page that did not look).
        Some(last.map_or_else(
            || again(q.before),
            |(sort_key, id)| SearchCursor {
                sort_key,
                id: MessageId(id),
                resume: None,
            },
        ))
    };
    let hits = found
        .into_iter()
        .map(|f| hit(core, f))
        .collect::<Result<Vec<_>>>()?;
    Ok(Walked {
        page: page(hits, next, capped, Some(SearchPattern(Arc::new(re)))),
        looked: scanned,
    })
}
