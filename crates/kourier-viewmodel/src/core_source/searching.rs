//! The message search of [`CoreSource`] (G1g): one search at a time, its pages, its reads
//! again, and what may show of it.
//!
//! - **One search, newest answer only.** A new text (or another chat) starts a new search: the
//!   tracker's floor rises, so an answer to an older text is dropped whenever it comes, and so is
//!   an answer to a page asked for it (one guard: the tracker's generations).
//! - **Pages.** The first page from the newest hit, then a page after the last one when the list
//!   asks ([`CoreSource::more_search_page`]). A page the core stopped early (`capped`: it looked
//!   at all it may, or ran out of time) that leaves fewer than a page shown goes on by itself;
//!   the core's walk ends (critic H4), so this does too.
//! - **Read again (critic H5).** Hits on screen follow their chats: an event that names a chat of
//!   the shown hits (or the chat a search is limited to) reads the hits again from the newest, as
//!   many as are shown, after [`REREAD_AFTER`] (F5, R3): the read goes into a BUFFER, page by page
//!   from the newest (`before` = the page's `next`) until it holds as many as were shown, the
//!   core has no more, or a bounded number of pages was read; only then the buffer replaces the
//!   hits (so a deleted message never stays, and a loaded row never drops meanwhile). The first
//!   such event arms the read, later ones join it, so a busy chat cannot hold it back. The poll
//!   of a filling index backs off (2 s, doubling, at most 30 s; a new text starts again).
//! - **A refinement keeps the rows (F6, R3).** A new text of the same scope keeps the shown hits
//!   (not paged, not "answered") until the first answer for the new text replaces them.
//! - **Fail closed (S-P).** A hit shows only while the list holds its chat and does not call it
//!   protected: a chat that turns protected, or leaves the list, takes its hits off at once,
//!   before any answer ([`CoreSource::search_rows_view`] reads the list each time; a list read
//!   applied tells the front-end). A chat removed and an account removed drop their hits from
//!   what is kept at once.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use kourier_core::facade::{
    MAX_SEARCH_PAGE, Query, QueryData, SearchCursor, SearchHit, SearchIndexState, SearchMode,
    SearchPattern, SearchQuery, UiCommand,
};
use kourier_model::{AccountId, ConvKind, ConversationId, MessageId, Redacted, UserId};

use super::{Changes, Completion, CoreSource, QueryToken, QueryTracker};
use crate::conversations::ConversationFilter;
use crate::search::{
    INDEX_REREAD_AFTER, MAX_CHAINED_PAGES, REREAD_AFTER, SEARCH_PAGE, SearchView, hit_rows,
    index_poll_delay,
};
use crate::source::UiSource;

/// Query model id of the message search (after `MODEL_COPY`).
pub const MODEL_SEARCH: u32 = 20;

/// What a search asks: the text as typed and the one chat it is limited to.
#[derive(Clone, PartialEq, Eq)]
struct Asked {
    text: Redacted<String>,
    chat: Option<ConversationId>,
    /// Words or a pattern (R5): the same text in another mode is another search.
    mode: SearchMode,
}

/// What one query in flight was sent for.
#[derive(Clone, Copy)]
struct Sent {
    /// A page after the last hit (its hits are appended), else a page of a read from the newest
    /// (the first, or one after it: [`Refresh`]).
    appending: bool,
}

/// A read from the newest that goes into a buffer (F5).
struct Refresh {
    /// How many hits were loaded when it began (0: a new text, its first page is the answer).
    want: usize,
    /// The pages read so far, newest first, without a message twice.
    buf: Vec<SearchHit>,
    /// Pages read, and the most it may read.
    pages: u32,
    max_pages: u32,
}

/// The search's state.
pub(super) struct Searching {
    asked: Option<Asked>,
    /// The mode the next search asks in (the field's `.*` and `Aa` toggles; kept when a search
    /// ends, set by [`CoreSource::set_search_mode`]).
    mode: SearchMode,
    /// The words of the core's refusal of a pattern the live check could not judge (too large
    /// to compile): shown under the field; empty otherwise.
    refusal: String,
    /// The compiled pattern of the last answer of a pattern search: the chat list's titles are
    /// filtered with it (nothing compiles on the Qt thread).
    pattern: Option<SearchPattern>,
    tracker: QueryTracker,
    /// What each query in flight was sent for, by generation (an entry goes with its answer,
    /// also an answer that is dropped).
    sent: HashMap<u64, Sent>,
    /// The hits loaded, newest first.
    hits: Vec<SearchHit>,
    /// Where the next page starts (`None`: no more hits).
    next: Option<SearchCursor>,
    /// How far the index is, as the last answer said.
    index: SearchIndexState,
    /// The last answer was capped.
    capped: bool,
    /// The core answered this search at least once.
    answered: bool,
    /// Capped pages asked in a row by the search itself ([`MAX_CHAINED_PAGES`]).
    chained: u32,
    /// A read from the newest is wanted (a new search, or its chats changed).
    fresh_wanted: bool,
    /// The read from the newest that is going on (its buffer), if any.
    refresh: Option<Refresh>,
    /// The next page of that read is wanted: where it starts.
    refresh_next: Option<SearchCursor>,
    /// Polls of a filling index armed for this text ([`index_poll_delay`]).
    index_step: u32,
    /// The page after the last hit is wanted.
    more_wanted: bool,
    /// When the hits are read again (a change of their chats, [`REREAD_AFTER`] later).
    reread_at: Option<Instant>,
    /// When the hits are read again because the index was still filling at the last answer
    /// ([`INDEX_REREAD_AFTER`] later). Kept apart from [`Searching::reread_at`]: it is a poll, no
    /// read the window waits for, so it does not count in [`Searching::pending`] (a picture taken
    /// while the index fills would never settle; the fixture page `searchindexing`, G1j).
    index_reread_at: Option<Instant>,
    /// How long a read again waits ([`REREAD_AFTER`]; the tests set it to 0).
    pub(super) debounce: Duration,
    /// How long after an answer with a filling index the hits are read again
    /// ([`INDEX_REREAD_AFTER`]; the tests set it to 0).
    pub(super) index_wait: Duration,
}

impl Searching {
    pub(super) fn new() -> Self {
        Self {
            asked: None,
            mode: SearchMode::Words,
            refusal: String::new(),
            pattern: None,
            tracker: QueryTracker::new(MODEL_SEARCH),
            sent: HashMap::new(),
            hits: Vec::new(),
            next: None,
            index: ready_index(),
            capped: false,
            answered: false,
            chained: 0,
            fresh_wanted: false,
            refresh: None,
            refresh_next: None,
            index_step: 0,
            more_wanted: false,
            reread_at: None,
            index_reread_at: None,
            debounce: REREAD_AFTER,
            index_wait: INDEX_REREAD_AFTER,
        }
    }

    /// Forgets the search and everything of it; answers still on their way are dropped.
    fn clear(&mut self) {
        self.asked = None;
        self.refusal.clear();
        self.pattern = None;
        self.tracker.reset();
        self.hits.clear();
        self.next = None;
        self.index = ready_index();
        self.capped = false;
        self.answered = false;
        self.chained = 0;
        self.fresh_wanted = false;
        self.refresh = None;
        self.refresh_next = None;
        self.index_step = 0;
        self.more_wanted = false;
        self.reread_at = None;
        self.index_reread_at = None;
    }

    /// A new text of the same scope: what shows stays (stale, not paged) until the first
    /// answer for the new text; everything else starts again (F6).
    fn refine(&mut self) {
        let hits = std::mem::take(&mut self.hits);
        let pattern = self.pattern.take();
        self.clear();
        self.hits = hits;
        self.pattern = pattern;
    }

    /// A read of the hits is on its way or waits for its time (a change's read again; the
    /// index's poll does not count).
    pub(super) fn pending(&self) -> bool {
        self.tracker.pending() || self.reread_at.is_some()
    }
}

/// The index state before any answer: nothing to say about it.
fn ready_index() -> SearchIndexState {
    SearchIndexState {
        ready: true,
        percent: 100,
    }
}

impl CoreSource {
    /// Searches the messages for `text`, in `chat` only or in every chat. The same text and chat
    /// again changes nothing; a text of white space alone ends the search.
    pub(super) fn start_search(&mut self, text: &str, chat: Option<ConversationId>) -> Changes {
        if text.trim().is_empty() {
            return self.end_search();
        }
        let asked = Asked {
            text: Redacted::new(text.to_owned()),
            chat,
            mode: self.search.mode,
        };
        if self.search.asked.as_ref() == Some(&asked) {
            return Changes::default();
        }
        // A refinement in the same scope keeps the rows until the first answer; a change of
        // scope (the chip on or off, another chat) clears at once (F6).
        let keep = self
            .search
            .asked
            .as_ref()
            .is_some_and(|a| a.chat == asked.chat && a.mode == asked.mode);
        if keep {
            self.search.refine();
        } else {
            self.search.clear();
        }
        self.search.asked = Some(asked);
        self.search.fresh_wanted = true;
        self.pump_search();
        Changes {
            search: true,
            ..Changes::default()
        }
    }

    /// The field's `.*` / `Aa` toggles: the mode the search asks in. A search that runs starts
    /// again in the new mode (its rows are another kind of hit, nothing is kept).
    pub(super) fn apply_search_mode(&mut self, mode: SearchMode) -> Changes {
        if self.search.mode == mode {
            return Changes::default();
        }
        self.search.mode = mode;
        let Some(asked) = self.search.asked.take() else {
            return Changes::default();
        };
        self.search.clear();
        let mut ch = self.start_search(asked.text.expose(), asked.chat);
        ch.search = true;
        ch
    }

    /// The compiled pattern of the running pattern search, once the core answered it.
    pub(super) fn search_pattern_now(&self) -> Option<SearchPattern> {
        self.search.pattern.clone()
    }

    /// The search ends: nothing shown, nothing asked.
    pub(super) fn end_search(&mut self) -> Changes {
        let was = self.search.asked.is_some();
        self.search.clear();
        Changes {
            search: was,
            ..Changes::default()
        }
    }

    /// The page after the last hit. `false`: no search, or no more hits.
    pub(super) fn more_search_page(&mut self) -> bool {
        let s = &mut self.search;
        // (Not while a read from the newest is on its way, nor before the first answer for the
        // text: the rows are not that text's. A capped search with no hits may be asked on: "Search
        // Further", F6.)
        if s.asked.is_none()
            || s.next.is_none()
            || !s.answered
            || s.refresh.is_some()
            || (s.hits.is_empty() && !s.capped)
        {
            return false;
        }
        s.chained = 0;
        s.more_wanted = true;
        self.pump_search();
        !self.stopped
    }

    /// Asks the core, or remembers to once the question in flight is answered.
    fn pump_search(&mut self) {
        let t = self.search.tracker.request();
        self.send_search(t);
    }

    /// Sends the wanted search query with `token`: a read from the newest first (as many hits
    /// as are loaded, at least a page), else the page after the last hit.
    fn send_search(&mut self, token: Option<QueryToken>) {
        let Some(token) = token else { return };
        let s = &mut self.search;
        let Some(asked) = s.asked.clone() else {
            s.tracker.reset();
            return;
        };
        let (before, limit, appending) = if s.fresh_wanted {
            s.fresh_wanted = false;
            s.refresh_next = None;
            // A text not answered yet wants no more than its first page (the rows kept from the
            // text before are not its own).
            let want = if s.answered { s.hits.len() } else { 0 };
            let shown = u32::try_from(want).unwrap_or(u32::MAX);
            let rounds = u32::try_from(want.div_ceil(MAX_SEARCH_PAGE as usize)).unwrap_or(u32::MAX);
            s.refresh = Some(Refresh {
                want,
                buf: Vec::new(),
                pages: 0,
                max_pages: MAX_CHAINED_PAGES.saturating_add(rounds),
            });
            (None, shown.clamp(SEARCH_PAGE, MAX_SEARCH_PAGE), false)
        } else if let Some(cursor) = s.refresh_next.take() {
            let missing = s
                .refresh
                .as_ref()
                .map_or(0, |r| r.want.saturating_sub(r.buf.len()));
            let missing = u32::try_from(missing).unwrap_or(u32::MAX);
            (
                Some(cursor),
                missing.clamp(SEARCH_PAGE, MAX_SEARCH_PAGE),
                false,
            )
        } else if s.more_wanted
            && let Some(next) = s.next
        {
            s.more_wanted = false;
            (Some(next), SEARCH_PAGE, true)
        } else {
            // Nothing is asked any more: the tracker must not wait for this token.
            s.more_wanted = false;
            s.tracker.reset();
            return;
        };
        s.sent.insert(token.generation, Sent { appending });
        // A page wanted while this read was wanted is still wanted: mark it again, so the
        // answer's follow-up sends it (as the gallery's slots do).
        if s.more_wanted {
            let _ = s.tracker.request();
        }
        self.send(UiCommand::Query {
            token,
            query: Query::Search(SearchQuery {
                text: asked.text,
                chats: asked.chat.map(|c| vec![c]),
                limit,
                before,
                mode: asked.mode,
            }),
        });
    }

    /// The answer of a search query.
    pub(super) fn on_search_result(
        &mut self,
        token: QueryToken,
        result: &Result<QueryData, std::sync::Arc<kourier_core::facade::CoreError>>,
    ) -> Changes {
        let mut ch = Changes::default();
        let sent = self.search.sent.remove(&token.generation);
        let applies = self.search.tracker.complete(token) == Completion::Apply;
        if applies
            && let Some(sent) = sent
            && self.search.asked.is_some()
        {
            match result {
                Ok(QueryData::Search(page)) => {
                    for h in &page.hits {
                        if let (Some(a), Some(n)) = (&h.author, &h.author_name) {
                            self.names.insert(a.clone(), n.expose().clone());
                        }
                    }
                    let s = &mut self.search;
                    if sent.appending {
                        let known: HashSet<_> = s.hits.iter().map(|h| h.message).collect();
                        s.hits.extend(
                            page.hits
                                .iter()
                                .filter(|h| !known.contains(&h.message))
                                .cloned(),
                        );
                    } else if let Some(r) = s.refresh.as_mut() {
                        // A page of the read from the newest: into the buffer.
                        let known: HashSet<_> = r.buf.iter().map(|h| h.message).collect();
                        r.buf.extend(
                            page.hits
                                .iter()
                                .filter(|h| !known.contains(&h.message))
                                .cloned(),
                        );
                        r.pages += 1;
                        if let Some(next) = page.next
                            && r.buf.len() < r.want
                            && r.pages < r.max_pages
                        {
                            // More is wanted: the shown hits stay until the buffer is whole.
                            s.refresh_next = Some(next);
                            s.index = page.index;
                            ch.search = true;
                            let t = s.tracker.follow_up();
                            if t.is_some() {
                                self.send_search(t);
                            } else if !self.search.tracker.in_flight() {
                                self.pump_search();
                            }
                            return ch;
                        }
                        if let Some(done) = s.refresh.take() {
                            s.hits = done.buf;
                        }
                    } else {
                        s.hits.clone_from(&page.hits);
                    }
                    if page.pattern.is_some() {
                        s.pattern.clone_from(&page.pattern);
                    }
                    s.refusal.clear();
                    s.next = page.next;
                    s.index = page.index;
                    s.capped = page.capped;
                    s.answered = true;
                    // G1i (call 3): an index that is still filling is asked about again, through
                    // the one re-read the App's wake-up already serves, so the percent moves and
                    // the hits of newly indexed old messages appear. A ready index arms nothing
                    // (a re-read a change of a shown chat armed stays).
                    if !page.index.ready && s.reread_at.is_none() && s.index_reread_at.is_none() {
                        let wait = index_poll_delay(s.index_wait, s.index_step);
                        s.index_step = s.index_step.saturating_add(1);
                        s.index_reread_at = Some(Instant::now() + wait);
                    }
                    // A page the core stopped early that leaves the list short goes on by
                    // itself (the list would have nothing to scroll for "more"), a bounded
                    // number of times in a row.
                    if !page.capped {
                        s.chained = 0;
                    } else if s.next.is_some()
                        && s.hits.len() < SEARCH_PAGE as usize
                        && s.chained < MAX_CHAINED_PAGES
                    {
                        s.chained += 1;
                        s.more_wanted = true;
                    }
                }
                // The core could not search (it is going away, or it refused the pattern): nothing
                // to show; a refusal says so under the field (the words, never the pattern).
                _ => {
                    let s = &mut self.search;
                    s.refusal = match result {
                        Err(e) => e
                            .filter_refusal()
                            .map_or_else(String::new, |r| crate::filters::refusal_note(&r).text),
                        Ok(_) => String::new(),
                    };
                    s.refresh = None;
                    s.refresh_next = None;
                    s.hits.clear();
                    s.next = None;
                    s.capped = false;
                    s.answered = false;
                }
            }
            ch.search = true;
        }
        let t = self.search.tracker.follow_up();
        if t.is_some() {
            self.send_search(t);
        } else if (self.search.more_wanted || self.search.refresh_next.is_some())
            && !self.search.tracker.in_flight()
        {
            self.pump_search();
        }
        ch
    }

    /// An event named chats: when `named` holds a chat of the shown hits (or the chat the
    /// search is limited to), the hits are read again after [`Searching::debounce`].
    pub(super) fn reread_search_if(&mut self, named: impl Fn(ConversationId) -> bool) {
        let s = &mut self.search;
        let Some(asked) = &s.asked else { return };
        let touched = asked.chat.is_some_and(&named) || s.hits.iter().any(|h| named(h.conv));
        if touched && s.reread_at.is_none() {
            s.reread_at = Some(Instant::now() + s.debounce);
        }
    }

    /// The same, for an account's changes (its contacts' names).
    pub(super) fn reread_search_of_account(&mut self, account: AccountId) {
        let s = &mut self.search;
        if s.asked.is_some() && s.hits.iter().any(|h| h.account == account) && s.reread_at.is_none()
        {
            s.reread_at = Some(Instant::now() + s.debounce);
        }
    }

    /// Reads the hits again from the newest (a lagged channel, or their time came).
    pub(super) fn reread_search(&mut self) {
        if self.search.asked.is_some() {
            self.search.reread_at = None;
            self.search.index_reread_at = None;
            self.search.fresh_wanted = true;
            self.pump_search();
        }
    }

    /// When the hits are read again, if a change waits for it.
    pub(super) fn search_reread_at(&self) -> Option<Instant> {
        self.search
            .reread_at
            .into_iter()
            .chain(self.search.index_reread_at)
            .min()
    }

    /// The read again may be due: sends it if so.
    pub(super) fn search_reread_due(&mut self) {
        if self
            .search_reread_at()
            .is_some_and(|at| at <= Instant::now())
        {
            self.reread_search();
        }
    }

    /// The chat `from` was merged into `into`: a search limited to `from` goes on in `into`.
    pub(super) fn merge_search_chat(&mut self, from: ConversationId, into: ConversationId) {
        if let Some(asked) = self.search.asked.as_mut()
            && asked.chat == Some(from)
        {
            asked.chat = Some(into);
        }
        self.reread_search_if(|c| c == from || c == into);
    }

    /// Drops the hits `gone` names (a removed account's or chat's) from what is kept, at once.
    /// `true` when one went. An answer on its way was read before the removal and may hold
    /// them again (privacy LOW-5): it is dropped when it comes, and the hits are read again
    /// from the newest, after the removal.
    pub(super) fn drop_search_hits(&mut self, gone: impl Fn(&SearchHit) -> bool) -> bool {
        let before = self.search.hits.len();
        self.search.hits.retain(|h| !gone(h));
        let s = &mut self.search;
        // (A buffer read before the removal may hold them: it is dropped with its answers.)
        let refreshing = s.refresh.take().is_some();
        s.refresh_next = None;
        if s.asked.is_some() && (s.tracker.pending() || refreshing) {
            s.tracker.reset();
            s.more_wanted = false;
            s.fresh_wanted = true;
            self.pump_search();
        }
        self.search.hits.len() != before
    }

    /// Whether a search shows hits (a list read changes what of them may show).
    pub(super) fn search_has_hits(&self) -> bool {
        self.search.asked.is_some() && !self.search.hits.is_empty()
    }

    /// What the search shows for the accounts `filter` leaves shown (their chips): every hit
    /// whose chat the list holds and does not call protected, newest first, as chat rows.
    pub(super) fn search_rows_view(&self, filter: &ConversationFilter) -> SearchView {
        let s = &self.search;
        let Some(asked) = &s.asked else {
            return SearchView::default();
        };
        let time = self.time();
        let hidden = self.resolve_hidden(filter);
        // (A search's rows name their account by the list's rule while searching.)
        let table = self.account_table(&hidden, true);
        // FAIL CLOSED: a chat the list does not hold (not read yet, hidden, removed, its account
        // going) or calls protected shows no hit, whatever the core answered.
        let listed: HashMap<ConversationId, &super::CoreConversation> = self
            .list
            .iter()
            .filter(|c| !c.protected)
            .map(|c| (c.id, c))
            .collect();
        let mut rows = hit_rows(
            &s.hits,
            |conv| listed.get(&conv).map(|c| self.conversation_row(c, &time)),
            &table,
            &self.names,
            &time,
        );
        // An own message shows the account's own picture as its sender's; a group member's hit
        // the picture of that member's 1:1 chat, the timeline's rule (`core_source.rs`, the
        // avatars of a window's rows), else initials.
        let authors: HashMap<MessageId, &UserId> = s
            .hits
            .iter()
            .filter_map(|h| h.author.as_ref().map(|a| (h.message, a)))
            .collect();
        let by_user: HashMap<(AccountId, &UserId), &super::CoreConversation> = self
            .list
            .iter()
            .filter(|c| c.kind == ConvKind::Direct && c.avatar.is_some())
            .filter_map(|c| c.peer.as_ref().map(|p| ((c.account, p), c)))
            .collect();
        for row in &mut rows {
            if row.own {
                if let Some(a) = self.accounts.iter().find(|a| a.id == row.account) {
                    row.sender_avatar_url = self.avatar_url(a.id, a.avatar.as_ref());
                }
            } else if row.kind != ConvKind::Direct
                && let Some(c) = authors
                    .get(&row.message)
                    .and_then(|u| by_user.get(&(row.account, *u)))
            {
                row.sender_avatar_url = self.avatar_url(row.account, c.avatar.as_ref());
            }
        }
        SearchView {
            active: true,
            chat: asked.chat,
            rows,
            more: s.next.is_some(),
            loading: s.tracker.pending(),
            // A pattern search scans the messages: the index being built is no news for it.
            index_ready: s.index.ready || asked.mode != SearchMode::Words,
            index_percent: s.index.percent,
            capped: s.capped,
            answered: s.answered,
            refusal: s.refusal.clone(),
            chat_gone: asked
                .chat
                .is_some_and(|c| !self.list.iter().any(|x| x.id == c)),
        }
    }
}
