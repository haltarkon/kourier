//! The search itself (G1e): [`Query::Search`](crate::facade::Query::Search) on the index G1b
//! to G1d write.
//!
//! THE MATCH is built from the terms of [`query_terms`] and nothing else: each term is quoted
//! (`"<token>"`, a `*` behind a prefix term), the terms are ANDed inside `words:( … )`, and a
//! scope is `meta:( "c<id>" OR … )` in the SAME expression, so FTS5 intersects the doclists and a
//! row of another chat is never stepped, never looked up (H5, H2's precondition). A term holds
//! ASCII letters and digits and bytes above 127 only (G1a's last split), so his `"`, `*`, `:`,
//! `(`, `NEAR`, `AND` are nothing or plain words, and the expression is bound as a parameter,
//! never formatted into the SQL. No term, or an EMPTY scope (`chats: Some(vec![])`), answers an
//! empty page: never every chat (M11, fail closed).
//!
//! THE WALK steps the index's rowids newest first (`rowid = hour << 36 | id`, G1b) on
//! search.db's connection, looks each id up in core.db's `messages` by primary key (core.db's
//! own read connection: the index may lag the messages, the query never shows the lag) with the
//! query-time rules ([`HIT`]: not deleted, not
//! view-once, not hidden by the sweeper, the chat not deleted and not protected, the account
//! not being removed; and the scope again, M11, for the lag of a merge), cuts its snippet
//! (`None` = stale or joined terms not side by side: dropped), and collects to the END of the
//! hour block in which the page filled; the block is sorted by `(sort_key, id)`, the page cut
//! to its limit. The cursor is the project's keyset `(sort_key, id)`: the next page starts at
//! that hour's end and drops the rows at or above it. Stateless, so rows that change between
//! pages do no harm.
//!
//! THE CAP (critic H4): a page looks up at most [`CAP`] rows and runs at most [`TIME_CAP`]
//! (and a newer search interrupts it, `Store::search`). A page that stops so answers what it
//! found (`capped`), and the hour it stopped in is given in id order (the rowid's order, which
//! inside an hour is the order of arrival): the cursor's `resume` says where the next page goes
//! on below, so every hit comes once and the walk ends, whatever the size of an hour. A capped
//! page ALWAYS has a `next` (privacy MEDIUM-2): one that stopped before it looked at any row
//! (a slow first step) gives the cursor it was asked with again, never a final "nothing"; so
//! does a search that found the index not ready, or a newer search asked ([`unanswered`]).
//!
//! Nothing here logs; the typed text is never in an error or a `Debug` (architecture §9.1).

use std::collections::HashSet;
use std::time::{Duration, Instant};

use kourier_db::rusqlite::{self, Connection, ErrorCode, OptionalExtension};
use kourier_model::{AccountId, Body, ConversationId, MessageId};

use crate::error::Result;
use crate::facade::{
    MAX_SEARCH_PAGE, SearchCursor, SearchHit, SearchIndexState, SearchMode, SearchPage, SearchQuery,
};
use crate::{query, sql};

use super::file;
use super::index::{self, ID_BITS, ID_MASK};
use super::normalize::{QueryTerm, query_terms};
use super::snippet::{Snippet, snippet};

/// The most rows of the index one page looks up (H4): a page past it answers what it found.
pub(crate) const CAP: usize = 5_000;

/// The longest one page runs (its walk checks the time before each row; a single step that
/// takes longer, a prefix's doclist built from every chat, is interrupted by `Store::search`).
pub(crate) const TIME_CAP: Duration = Duration::from_secs(1);

/// The index's rowids that match `?1`, at or below `?2`, newest first. FTS5 serves the order
/// and the bound itself (no sort: the plan test).
pub(crate) const ROWIDS: &str = "SELECT rowid FROM messages_fts
     WHERE messages_fts MATCH ?1 AND rowid <= ?2 ORDER BY rowid DESC";

/// The query-time rules of a hit, written ONCE for the word search ([`HIT`]) and the pattern
/// scan (`pattern::ID_SCAN`, `TIMELINE_SCAN`): not deleted, not view-once, not hidden by the
/// sweeper, the chat not deleted and not protected, the account not being removed.
macro_rules! hit_rules {
    () => {
        "m.deleted IS NULL AND m.view_once = 0 AND m.expiry_hidden = 0
       AND c.hidden = 0 AND c.protected = 0 AND a.state <> 'Removing'"
    };
}
pub(crate) use hit_rules;

/// One candidate by its id, with THE QUERY-TIME RULES (what can flip after the row was indexed
/// is never decided by the index): not deleted (for everyone or for me), not view-once, not
/// hidden on an unconfirmed clock, the chat not deleted here (`hidden`) and NOT PROTECTED (S-P:
/// hits show outside the open chat, in the list, `.claude/rules/core.md`), the account not
/// being removed (the list's own rule, L11: an `Unlinking` account's chats are listed, so they
/// are searched). Every table by primary key, in this order (`CROSS JOIN`: the plan test).
pub(crate) const HIT: &str = concat!(
    "SELECT m.conversation_id, c.account_id, m.sort_key, m.sent_at,
       m.net_author, m.body
     FROM messages m
     CROSS JOIN conversations c ON c.id = m.conversation_id
     CROSS JOIN accounts a ON a.id = c.account_id
     WHERE m.id = ?1 AND ",
    hit_rules!()
);

/// The FTS5 expression of `terms` in `chats` (`None`: every chat), or `None` when nothing can
/// match: no term, or an empty list of chats.
pub(crate) fn match_expr(terms: &[QueryTerm], chats: Option<&[ConversationId]>) -> Option<String> {
    if terms.is_empty() {
        return None;
    }
    let mut out = String::from("words:(");
    for (i, term) in terms.iter().enumerate() {
        if i > 0 {
            out.push_str(" AND ");
        }
        out.push('"');
        out.push_str(&term.text);
        out.push('"');
        if term.prefix {
            out.push('*');
        }
    }
    out.push(')');
    match chats {
        None => {}
        // A grant of no chat is no grant: never the whole index.
        Some([]) => return None,
        Some(chats) => {
            out.push_str(" AND meta:(");
            for (i, conv) in chats.iter().enumerate() {
                if i > 0 {
                    out.push_str(" OR ");
                }
                out.push_str("\"c");
                out.push_str(&conv.0.to_string());
                out.push('"');
            }
            out.push(')');
        }
    }
    Some(out)
}

/// How far the index is ([`file::progress`], on search.db).
pub(crate) fn state(search: &Connection) -> Result<SearchIndexState> {
    let (ready, percent) = file::progress(search)?;
    Ok(SearchIndexState { ready, percent })
}

/// The cursor that asks a page again: the one it was asked with (`before`), or the newest end
/// of the index (`hour` and id at their largest: the same walk as no cursor).
pub(super) fn again(before: Option<SearchCursor>) -> SearchCursor {
    before.unwrap_or(SearchCursor {
        sort_key: i64::MAX,
        id: MessageId(i64::MAX),
        resume: None,
    })
}

/// The answer of a search that did not run (the index file not judged yet, or a newer search
/// asked meanwhile): no hit, `capped`, and the same page as its `next`, so the viewmodel asks on
/// and never shows a final "nothing" (privacy MEDIUM-2, MEDIUM-3).
pub(crate) fn unanswered(before: Option<SearchCursor>) -> SearchPage {
    SearchPage {
        hits: Vec::new(),
        next: Some(again(before)),
        capped: true,
        index: SearchIndexState {
            ready: false,
            percent: 0,
        },
        pattern: None,
    }
}

/// The last rowid of an hour block.
fn block_end(block: i64) -> i64 {
    (block << ID_BITS) | ID_MASK
}

/// Whether a statement was stopped by `sqlite3_interrupt` (a newer search, or the time cap).
pub(super) fn interrupted(e: &rusqlite::Error) -> bool {
    e.sqlite_error_code() == Some(ErrorCode::OperationInterrupted)
}

/// A hit found on the walk, before the page is cut.
pub(super) struct Found {
    pub(super) rowid: i64,
    pub(super) id: i64,
    pub(super) conv: i64,
    pub(super) account: i64,
    pub(super) sort_key: i64,
    pub(super) sent_at: i64,
    pub(super) author: String,
    pub(super) snippet: Snippet,
}

/// The hits of one hour block, in the walk's order (rowid, that is id, descending).
struct Block {
    block: i64,
    /// Given in id order: the block the cursor resumes in, or the one the page stopped in.
    by_id: bool,
    found: Vec<Found>,
}

/// A page and how many rows of the index it looked up (the scope test counts them).
pub(crate) struct Walked {
    pub(crate) page: SearchPage,
    pub(crate) looked: usize,
}

/// One page of `q`: the walk on `search` (search.db), each row looked up on `core` (core.db),
/// at most `cap` rows looked up, the walk stopped at `deadline`.
pub(crate) fn search(
    search: &Connection,
    core: &Connection,
    q: &SearchQuery,
    cap: usize,
    deadline: Instant,
) -> Result<Walked> {
    let limit = usize::try_from(q.limit.clamp(1, MAX_SEARCH_PAGE)).unwrap_or(1);
    let index = state(search)?;
    if let SearchMode::Pattern { match_case } = q.mode {
        return super::pattern::scan(core, q, match_case, index, deadline);
    }
    let terms = query_terms(q.text.expose());
    let empty = |looked| Walked {
        page: SearchPage {
            hits: Vec::new(),
            next: None,
            capped: false,
            index,
            pattern: None,
        },
        looked,
    };
    let Some(expr) = match_expr(&terms, q.chats.as_deref()) else {
        return Ok(empty(0));
    };
    let scope: Option<HashSet<i64>> = q.chats.as_ref().map(|c| c.iter().map(|c| c.0).collect());
    // Where the walk starts, what it leaves out, and the block it resumes in id order.
    let (hi, bound, resume_block) = match q.before {
        None => (i64::MAX, (i64::MAX, i64::MAX), None),
        Some(c) => match c.resume {
            Some(r) => (
                r.saturating_sub(1),
                (c.sort_key, c.id.0),
                Some(r >> ID_BITS),
            ),
            None => (
                block_end(index::hour(c.sort_key)),
                (c.sort_key, c.id.0),
                None,
            ),
        },
    };
    let mut rowids = search.prepare_cached(ROWIDS)?;
    let mut lookup = core.prepare_cached(HIT)?;
    let mut rows = rowids.query((expr.as_str(), hi))?;
    let mut blocks: Vec<Block> = Vec::new();
    let (mut looked, mut count) = (0_usize, 0_usize);
    let mut last_looked: Option<i64> = None;
    // `Some(true)`: the walk stopped inside the last block (it was not walked to its end).
    let mut stopped_inside: Option<bool> = None;
    let mut exhausted = false;
    loop {
        let rowid: i64 = match rows.next() {
            Ok(Some(row)) => row.get(0)?,
            Ok(None) => {
                exhausted = true;
                break;
            }
            Err(e) if interrupted(&e) => {
                stopped_inside = Some(true);
                break;
            }
            Err(e) => return Err(e.into()),
        };
        let block = rowid >> ID_BITS;
        let same = blocks.last().is_some_and(|b| b.block == block);
        // The page is full: at the end of the block it filled in (a block in id order needs
        // no end: its rows already come in the page's order).
        if count >= limit && blocks.last().is_some_and(|b| b.by_id || !same) {
            break;
        }
        if looked >= cap || Instant::now() >= deadline {
            stopped_inside = Some(same);
            break;
        }
        let id = rowid & ID_MASK;
        type Raw = (i64, i64, i64, i64, String, String);
        let raw: Option<Raw> = match lookup
            .query_row([id], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            })
            .optional()
        {
            Ok(raw) => raw,
            Err(e) if interrupted(&e) => {
                stopped_inside = Some(same);
                break;
            }
            Err(e) => return Err(e.into()),
        };
        looked += 1;
        last_looked = Some(rowid);
        if !same {
            blocks.push(Block {
                block,
                by_id: resume_block == Some(block),
                found: Vec::new(),
            });
        }
        let Some((conv, account, sort_key, sent_at, author, body)) = raw else {
            continue;
        };
        if scope.as_ref().is_some_and(|s| !s.contains(&conv)) || (sort_key, id) >= bound {
            continue;
        }
        // A body that does not parse is not searchable (M9); a snippet of `None` is a stale
        // row or joined terms that do not stand side by side.
        let Ok(body) = sql::from_json::<Body>(&body) else {
            continue;
        };
        let Some(snippet) = snippet(&body, &terms) else {
            continue;
        };
        if let Some(b) = blocks.last_mut() {
            b.found.push(Found {
                rowid,
                id,
                conv,
                account,
                sort_key,
                sent_at,
                author,
                snippet,
            });
            count += 1;
        }
    }
    drop(rows);
    let capped = stopped_inside.is_some();
    if stopped_inside == Some(true)
        && let Some(b) = blocks.last_mut()
    {
        b.by_id = true;
    }
    let mut all: Vec<(Found, bool)> = Vec::with_capacity(count);
    for b in blocks {
        let mut found = b.found;
        if !b.by_id {
            found.sort_unstable_by_key(|x| std::cmp::Reverse((x.sort_key, x.id)));
        }
        all.extend(found.into_iter().map(|f| (f, b.by_id)));
    }
    let after = |rowid: i64| SearchCursor {
        sort_key: bound.0,
        id: MessageId(bound.1),
        resume: Some(rowid),
    };
    let next = if all.len() > limit {
        let (last, by_id) = &all[limit - 1];
        Some(if *by_id {
            after(last.rowid)
        } else {
            SearchCursor {
                sort_key: last.sort_key,
                id: MessageId(last.id),
                resume: None,
            }
        })
    } else if exhausted {
        None
    } else {
        // Every hit found was given: the walk goes on below the last row it looked at. A page
        // that looked at nothing (stopped at its first step) asks its own page again: never a
        // final "nothing" for a page that did not look (privacy MEDIUM-2).
        Some(last_looked.map_or_else(|| again(q.before), after))
    };
    all.truncate(limit);
    let hits = all
        .into_iter()
        .map(|(f, _)| hit(core, f))
        .collect::<Result<Vec<_>>>()?;
    Ok(Walked {
        page: SearchPage {
            hits,
            next,
            capped,
            index,
            pattern: None,
        },
        looked,
    })
}

/// The hit of a found row: its author and the core's name for them.
pub(super) fn hit(conn: &Connection, f: Found) -> Result<SearchHit> {
    let own = f.author.is_empty();
    let author = (!own).then(|| sql::user(&f.author)).transpose()?;
    let author_name = if own {
        None
    } else {
        match query::display_name(conn, f.account, &f.author) {
            Ok(name) => name,
            // A newer search took the connection: the page is dropped anyway.
            Err(crate::error::CoreError::Sql(e)) if interrupted(&e) => None,
            Err(e) => return Err(e),
        }
    };
    Ok(SearchHit {
        message: MessageId(f.id),
        conv: ConversationId(f.conv),
        account: AccountId(f.account),
        sort_key: f.sort_key,
        sent_at: f.sent_at,
        own,
        author,
        author_name: author_name.map(Into::into),
        snippet: f.snippet,
    })
}

#[cfg(test)]
mod tests;
