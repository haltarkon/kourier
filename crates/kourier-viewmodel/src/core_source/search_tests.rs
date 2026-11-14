//! `CoreSource` and the message search (G1g), against the simulated core of [`super::tests`]:
//! the rows of the hits, a stale answer dropped, the pages, the reads again after a change of a
//! shown hit's chat (critic H5), the hits that fail closed (S-P), and `open_at` (critic B1, L1):
//! the window around the hit is asked before any page at the newest messages, so an old hit marks
//! nothing read.

use std::cmp::Reverse;
use std::time::{Duration, Instant};

use kourier_core::facade::{
    Query, QueryData, QueryToken, SearchCursor, SearchHit, SearchIndexState, SearchMode,
    SearchPage, SearchPattern, SearchQuery, Snippet, SnippetMark, TimelineAnchor, TimelineQuery,
    UiCommand, UiEvent,
};
use kourier_model::{AccountId, ConversationId, MessageId, Redacted};

use super::tests::{NOW, Sent, Store, message, result, source, take_queries, user};
use super::*;
use crate::search::SEARCH_PAGE;
use crate::source::{EmptySource, FixtureSource};

const C1: ConversationId = ConversationId(1);
const C2: ConversationId = ConversationId(2);

/// A hit of message `n` of chat `conv` (the id and sort key of the simulated core's
/// `message(conv, n)`), its snippet `text`, "plan" marked.
fn hit(conv: i64, n: i64, text: &str) -> SearchHit {
    SearchHit {
        message: MessageId(conv * 100_000 + n),
        conv: ConversationId(conv),
        account: AccountId(1),
        sort_key: n * 10,
        sent_at: NOW - 3_600_000 + n * 1000,
        own: false,
        author: Some(user(1)),
        author_name: Some(Redacted::new("Ari Arden".to_owned())),
        snippet: Snippet {
            text: text.to_owned(),
            marks: text
                .find("plan")
                .map(|start| SnippetMark {
                    start: start as u32,
                    len: 4,
                })
                .into_iter()
                .collect(),
            ranges: Vec::new(),
            file: None,
        },
    }
}

/// The simulated search index: the hits whose snippet holds every typed word, newest first by
/// `(sort_key, id)`, in the chats asked, after the cursor; `cap` makes every page stop early
/// (`capped`) after that many hits.
#[derive(Clone, Default)]
struct Db {
    hits: Vec<SearchHit>,
    cap: Option<usize>,
    index: Option<SearchIndexState>,
    /// Every page stops before its first hit (a slow first step): no hit, capped, and the
    /// same page again as its `next`.
    stall: bool,
}

impl Db {
    fn new(hits: Vec<SearchHit>) -> Self {
        Self {
            hits,
            ..Self::default()
        }
    }

    fn page(&self, q: &SearchQuery) -> SearchPage {
        if self.stall {
            return SearchPage {
                hits: Vec::new(),
                next: Some(q.before.unwrap_or(SearchCursor {
                    sort_key: i64::MAX,
                    id: MessageId(i64::MAX),
                    resume: None,
                })),
                capped: true,
                index: SearchIndexState {
                    ready: true,
                    percent: 100,
                },
                pattern: None,
            };
        }
        let words: Vec<String> = q
            .text
            .expose()
            .split_whitespace()
            .map(str::to_lowercase)
            .collect();
        let mut found: Vec<&SearchHit> = self
            .hits
            .iter()
            .filter(|h| q.chats.as_ref().is_none_or(|c| c.contains(&h.conv)))
            .filter(|h| {
                let text = h.snippet.text.to_lowercase();
                words.iter().all(|w| text.contains(w.as_str()))
            })
            .filter(|h| {
                q.before
                    .is_none_or(|c| (h.sort_key, h.message) < (c.sort_key, c.id))
            })
            .collect();
        found.sort_by_key(|h| Reverse((h.sort_key, h.message)));
        let limit = self.cap.unwrap_or(usize::MAX).min(q.limit as usize);
        let more = found.len() > limit;
        let hits: Vec<SearchHit> = found.into_iter().take(limit).cloned().collect();
        SearchPage {
            next: if more {
                hits.last().map(|h| SearchCursor {
                    sort_key: h.sort_key,
                    id: h.message,
                    resume: None,
                })
            } else {
                None
            },
            hits,
            capped: self.cap.is_some() && more,
            index: self.index.unwrap_or(SearchIndexState {
                ready: true,
                percent: 100,
            }),
            // A pattern search hands its compiled pattern back (the core compiles it).
            pattern: match q.mode {
                SearchMode::Pattern { match_case } => {
                    kourier_core::facade::filters::compile(q.text.expose(), match_case)
                        .ok()
                        .map(|re| SearchPattern(std::sync::Arc::new(re)))
                }
                SearchMode::Words => None,
            },
        }
    }
}

/// Answers every query (and what follows them) in order: the searches from `db`, the rest from
/// `store`. Returns the changes and the searches asked.
fn answer_all(
    src: &mut CoreSource,
    sent: &Sent,
    store: &Store,
    db: &Db,
) -> (Changes, Vec<SearchQuery>) {
    let mut ch = Changes::default();
    let mut asked = Vec::new();
    for _ in 0..100 {
        let qs = take_queries(sent);
        if qs.is_empty() {
            return (ch, asked);
        }
        for (t, q) in qs {
            let data = match &q {
                Query::Search(s) => {
                    asked.push(s.clone());
                    QueryData::Search(db.page(s))
                }
                q => store.answer(q),
            };
            ch.merge(src.on_event(&result(t, data)));
        }
    }
    panic!("queries do not settle");
}

/// Answers the queries that are not searches; the searches are taken and returned unanswered.
fn answer_but_searches(
    src: &mut CoreSource,
    sent: &Sent,
    store: &Store,
) -> (Changes, Vec<(QueryToken, SearchQuery)>) {
    let mut ch = Changes::default();
    let mut held = Vec::new();
    for _ in 0..100 {
        let qs = take_queries(sent);
        if qs.is_empty() {
            return (ch, held);
        }
        for (t, q) in qs {
            match q {
                Query::Search(s) => held.push((t, s)),
                q => ch.merge(src.on_event(&result(t, store.answer(&q)))),
            }
        }
    }
    panic!("queries do not settle");
}

/// A source with the account and the list read.
fn opened(store: &Store) -> (CoreSource, Sent) {
    let (mut src, sent) = source();
    src.reload();
    answer_all(&mut src, &sent, store, &Db::default());
    (src, sent)
}

fn shown(src: &CoreSource) -> Vec<MessageId> {
    src.search_view(&ConversationFilter::default())
        .rows
        .iter()
        .map(|r| r.message)
        .collect()
}

fn mark_reads(sent: &Sent) -> usize {
    sent.borrow()
        .iter()
        .filter(|c| matches!(c, UiCommand::MarkRead { .. }))
        .count()
}

/// The two chats of the list, and hits in both (and one that does not match).
fn world() -> (Store, Db) {
    (
        Store::new(&[(1, 10), (2, 5)]),
        Db::new(vec![
            hit(1, 3, "bring the plan"),
            hit(2, 4, "the plan is set"),
            hit(1, 1, "nothing here"),
        ]),
    )
}

/// The hits show under the chats as chat rows, newest first; the query is the text as typed for
/// every chat, or one chat; the same text again asks nothing.
#[test]
fn a_search_shows_its_hits_as_rows_of_their_chats() {
    let (store, db) = world();
    let (mut src, sent) = opened(&store);
    let ch = src.search_messages("PLAN", None);
    assert!(ch.search);
    let view = src.search_view(&ConversationFilter::default());
    assert!(view.active && view.loading && view.rows.is_empty());
    assert_eq!(
        src.queries_in_flight(),
        1,
        "a screenshot waits for the hits"
    );
    let (ch, asked) = answer_all(&mut src, &sent, &store, &db);
    assert!(ch.search);
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0].text.expose(), "PLAN");
    assert_eq!(
        (asked[0].chats.clone(), asked[0].limit, asked[0].before),
        (None, SEARCH_PAGE, None)
    );
    let view = src.search_view(&ConversationFilter::default());
    assert!(!view.loading && !view.more && view.index_ready);
    assert_eq!(src.queries_in_flight(), 0);
    assert_eq!(shown(&src), vec![MessageId(200_004), MessageId(100_003)]);
    let row = &view.rows[0];
    assert_eq!(row.title, "c2 v0");
    assert_eq!(row.author, "Ari");
    assert_eq!(row.snippet.html, "the <b>plan</b> is set");
    // The same text and chat again: nothing new.
    assert!(!src.search_messages("PLAN", None).search);
    assert!(take_queries(&sent).is_empty());
    // One chat.
    assert!(src.search_messages("PLAN", Some(C2)).search);
    let (_, asked) = answer_all(&mut src, &sent, &store, &db);
    assert_eq!(asked[0].chats, Some(vec![C2]));
    let view = src.search_view(&ConversationFilter::default());
    assert_eq!(view.chat, Some(C2));
    assert_eq!(shown(&src), vec![MessageId(200_004)]);
    // The index's state comes with the answers.
    let db = Db {
        index: Some(SearchIndexState {
            ready: false,
            percent: 40,
        }),
        ..db
    };
    src.search_messages("plan", None);
    answer_all(&mut src, &sent, &store, &db);
    let view = src.search_view(&ConversationFilter::default());
    assert_eq!((view.index_ready, view.index_percent), (false, 40));
}

/// A new text asks at once, whatever is in flight, and an answer to the older text is dropped
/// whenever it comes: after the newer one, or before it.
#[test]
fn a_stale_search_answer_is_dropped() {
    let (store, db) = world();
    let (mut src, sent) = opened(&store);
    let old_page = |n: i64| {
        QueryData::Search(SearchPage {
            hits: vec![hit(1, n, "an old plan")],
            next: None,
            capped: false,
            index: SearchIndexState {
                ready: true,
                percent: 100,
            },
            pattern: None,
        })
    };
    src.search_messages("pl", None);
    let first = take_queries(&sent);
    src.search_messages("plan is", None);
    let second = take_queries(&sent);
    assert_eq!(
        (first.len(), second.len()),
        (1, 1),
        "the new text asks at once"
    );
    let (t2, q2) = second[0].clone();
    let Query::Search(q2) = q2 else {
        panic!("a search")
    };
    src.on_event(&result(t2, QueryData::Search(db.page(&q2))));
    assert_eq!(shown(&src), vec![MessageId(200_004)]);
    let ch = src.on_event(&result(first[0].0, old_page(7)));
    assert!(!ch.search, "the older answer changes nothing");
    assert_eq!(shown(&src), vec![MessageId(200_004)]);
    // The older answer first, then the newer.
    src.search_messages("bring", None);
    let third = take_queries(&sent);
    src.search_messages("bring the", None);
    let fourth = take_queries(&sent);
    src.on_event(&result(third[0].0, old_page(8)));
    let view = src.search_view(&ConversationFilter::default());
    // (F6: the rows of the text before stay while the newer answer is awaited; the stale
    // answer's own hit does not show.)
    assert!(
        view.loading && !view.answered && !shown(&src).contains(&MessageId(100_008)),
        "still waiting for the newer"
    );
    let Query::Search(q4) = fourth[0].1.clone() else {
        panic!("a search")
    };
    src.on_event(&result(fourth[0].0, QueryData::Search(db.page(&q4))));
    assert_eq!(shown(&src), vec![MessageId(100_003)]);
    // An ended search drops a late answer too.
    src.search_messages("plan", None);
    let late = take_queries(&sent);
    assert!(src.close_search().search);
    assert!(!src.on_event(&result(late[0].0, old_page(9))).search);
    assert_eq!(
        src.search_view(&ConversationFilter::default()),
        crate::search::SearchView::default()
    );
    assert!(!src.close_search().search, "nothing more to end");
}

/// Pages after the last hit are appended; a page the core stopped early that leaves the list
/// short goes on by itself until the walk ends.
#[test]
fn search_pages_append_and_a_short_capped_page_goes_on() {
    let store = Store::new(&[(1, 10)]);
    let db = Db::new((1..=120).map(|n| hit(1, n, &format!("plan {n}"))).collect());
    let (mut src, sent) = opened(&store);
    src.search_messages("plan", None);
    answer_all(&mut src, &sent, &store, &db);
    let view = src.search_view(&ConversationFilter::default());
    assert_eq!((view.rows.len(), view.more), (SEARCH_PAGE as usize, true));
    assert!(src.more_search());
    let (_, asked) = answer_all(&mut src, &sent, &store, &db);
    assert_eq!(
        asked[0].before.map(|c| c.id),
        Some(MessageId(100_000 + 120 - i64::from(SEARCH_PAGE) + 1))
    );
    assert!(src.more_search());
    answer_all(&mut src, &sent, &store, &db);
    let view = src.search_view(&ConversationFilter::default());
    assert_eq!((view.rows.len(), view.more), (120, false));
    assert_eq!(view.rows[0].message, MessageId(100_120));
    assert_eq!(view.rows[119].message, MessageId(100_001));
    assert!(!src.more_search(), "no more hits");
    // The core stops each page after 3 hits: the search goes on by itself, one page at a time.
    let db = Db {
        cap: Some(3),
        ..Db::new((1..=10).map(|n| hit(1, n, "plan")).collect())
    };
    src.search_messages("plan", Some(C1));
    let (_, asked) = answer_all(&mut src, &sent, &store, &db);
    assert_eq!(asked.len(), 4, "3 + 3 + 3 + 1");
    assert_eq!(
        src.search_view(&ConversationFilter::default()).rows.len(),
        10
    );
}

/// MEDIUM-2: capped pages go on by themselves at most [`crate::search::MAX_CHAINED_PAGES`] in a
/// row; the view says `capped` (never a final "nothing"); a page he asks for starts the count
/// again, and so does an answer that is not capped.
#[test]
fn a_chain_of_capped_pages_stops_at_its_bound_and_the_view_says_capped() {
    use crate::search::MAX_CHAINED_PAGES;
    let store = Store::new(&[(1, 10)]);
    let chain = MAX_CHAINED_PAGES as usize;
    let db = Db {
        cap: Some(1),
        ..Db::new((1..=40).map(|n| hit(1, n, "plan")).collect())
    };
    let (mut src, sent) = opened(&store);
    src.search_messages("plan", None);
    let (_, asked) = answer_all(&mut src, &sent, &store, &db);
    assert_eq!(asked.len(), 1 + chain, "the first page and the chain");
    let view = src.search_view(&ConversationFilter::default());
    assert_eq!(view.rows.len(), 1 + chain);
    assert!(view.capped && view.more);
    assert!(src.more_search(), "he asks for more");
    let (_, asked) = answer_all(&mut src, &sent, &store, &db);
    assert_eq!(asked.len(), 1 + chain, "his page starts the count again");
    assert_eq!(
        src.search_view(&ConversationFilter::default()).rows.len(),
        2 * (1 + chain)
    );
    // A first step that never ends in time: no hit, and the chain still ends.
    let stalled = Db {
        stall: true,
        ..Db::default()
    };
    src.search_messages("pl", None);
    let (_, asked) = answer_all(&mut src, &sent, &store, &stalled);
    assert_eq!(asked.len(), 1 + chain);
    let view = src.search_view(&ConversationFilter::default());
    assert!(
        view.rows.is_empty() && view.capped,
        "still searching, not no results"
    );
    // An answer that is not capped: the view says so.
    src.search_messages("plan", Some(C1));
    answer_all(&mut src, &sent, &store, &Db::new(vec![hit(1, 2, "plan")]));
    let view = src.search_view(&ConversationFilter::default());
    assert_eq!(view.rows.len(), 1);
    assert!(!view.capped && !view.more);
}

/// H5: a change of a chat of the shown hits reads them again, from the newest, as many as are
/// shown, [`crate::search::REREAD_AFTER`] after the first change; later changes join it, and a
/// change of another chat arms nothing.
#[test]
fn a_change_of_a_shown_hits_chat_reads_the_search_again_after_a_moment() {
    let (store, db) = world();
    let (mut src, sent) = opened(&store);
    src.search_messages("plan", None);
    answer_all(&mut src, &sent, &store, &db);
    // A chat with no hit shown: nothing (chat 3 is not in the world).
    src.on_event(&UiEvent::MessagesChanged {
        conv: ConversationId(3),
        ids: vec![],
    });
    assert_eq!(src.search_deadline(), None);
    let before = Instant::now();
    src.on_event(&UiEvent::MessagesChanged {
        conv: C1,
        ids: vec![MessageId(100_003)],
    });
    let at = src.search_deadline().expect("a read again is armed");
    assert!(
        at >= before + Duration::from_millis(450),
        "it waits a moment"
    );
    assert!(at <= Instant::now() + Duration::from_millis(500));
    assert!(
        take_queries(&sent)
            .iter()
            .all(|(_, q)| !matches!(q, Query::Search(_)))
    );
    src.on_event(&UiEvent::TimelineInvalidated { conv: C2 });
    assert_eq!(src.search_deadline(), Some(at), "a later change joins it");
    src.search_due();
    assert!(take_queries(&sent).is_empty(), "not before its time");
    assert_eq!(src.queries_in_flight(), 1, "the rows are going to change");
    std::thread::sleep(at.saturating_duration_since(Instant::now()) + Duration::from_millis(5));
    src.search_due();
    assert_eq!(src.search_deadline(), None);
    let (_, asked) = answer_all(&mut src, &sent, &store, &db);
    assert_eq!(asked.len(), 1);
    assert_eq!((asked[0].before, asked[0].limit), (None, SEARCH_PAGE));
}

/// G1i (call 3): an answer whose index is still filling arms ONE re-read at the named constant
/// through the same deadline the H5 re-read uses; an answer with a ready index arms nothing; the
/// re-read asks the pages again and the percent of its answer is what the view shows.
#[test]
fn a_filling_index_arms_a_re_read_and_a_ready_one_arms_nothing() {
    use crate::search::INDEX_REREAD_AFTER;
    let (store, mut db) = world();
    // A ready index: nothing is armed, and the view says answered.
    let (mut src, sent) = opened(&store);
    src.search_messages("plan", None);
    assert!(!src.search_view(&ConversationFilter::default()).answered);
    answer_all(&mut src, &sent, &store, &db);
    assert_eq!(src.search_deadline(), None, "a ready index arms nothing");
    assert!(src.search_view(&ConversationFilter::default()).answered);
    // A filling index: the re-read is armed at the constant, once.
    db.index = Some(SearchIndexState {
        ready: false,
        percent: 20,
    });
    let before = Instant::now();
    src.search_messages("plan is", None);
    answer_all(&mut src, &sent, &store, &db);
    let at = src
        .search_deadline()
        .expect("a filling index arms a re-read");
    assert!(at >= before + INDEX_REREAD_AFTER, "it waits the constant");
    assert!(at <= Instant::now() + INDEX_REREAD_AFTER);
    let view = src.search_view(&ConversationFilter::default());
    assert_eq!((view.index_ready, view.index_percent), (false, 20));
    assert_eq!(
        src.queries_in_flight(),
        0,
        "the index's poll holds no picture (G1j: `searchindexing` would never settle)"
    );
    // The re-read (at once here) asks the pages again, and the next answer's percent shows.
    src.search.index_wait = Duration::ZERO;
    src.search_messages("plan the", None);
    answer_all(&mut src, &sent, &store, &db);
    assert!(src.search_deadline().is_some());
    src.search_due();
    db.index = Some(SearchIndexState {
        ready: false,
        percent: 60,
    });
    let (_, asked) = answer_all(&mut src, &sent, &store, &db);
    assert_eq!(asked.len(), 1);
    assert_eq!((asked[0].before, asked[0].limit), (None, SEARCH_PAGE));
    let view = src.search_view(&ConversationFilter::default());
    assert_eq!((view.index_ready, view.index_percent), (false, 60));
    assert!(
        src.search_deadline().is_some(),
        "still filling: armed again"
    );
    // The index is ready at the next answer: the line's state ends and nothing is armed.
    src.search_due();
    db.index = Some(SearchIndexState {
        ready: true,
        percent: 100,
    });
    answer_all(&mut src, &sent, &store, &db);
    assert_eq!(src.search_deadline(), None);
    assert!(src.search_view(&ConversationFilter::default()).index_ready);
}

/// A source with the hits of [`world`] on screen and reads again at once (`search_due`).
fn on_screen() -> (CoreSource, Sent, Store, Db) {
    let (store, db) = world();
    let (mut src, sent) = opened(&store);
    src.search.debounce = Duration::ZERO;
    src.search_messages("plan", None);
    answer_all(&mut src, &sent, &store, &db);
    assert_eq!(shown(&src), vec![MessageId(200_004), MessageId(100_003)]);
    (src, sent, store, db)
}

/// H5: a disappearing message whose timer ran out leaves the hits on screen.
#[test]
fn an_expired_message_leaves_the_search_hits_on_screen() {
    let (mut src, sent, store, mut db) = on_screen();
    db.hits.retain(|h| h.message != MessageId(100_003));
    src.on_event(&UiEvent::MessagesChanged {
        conv: C1,
        ids: vec![MessageId(100_003)],
    });
    let ch = src.search_due();
    let (more, _) = answer_all(&mut src, &sent, &store, &db);
    assert!(ch.search || more.search);
    assert_eq!(shown(&src), vec![MessageId(200_004)]);
}

/// H5: a message deleted for everyone (and a chat cleared) leaves the hits on screen.
#[test]
fn a_message_deleted_for_everyone_leaves_the_search_hits_on_screen() {
    let (mut src, sent, store, mut db) = on_screen();
    db.hits.retain(|h| h.message != MessageId(200_004));
    src.on_event(&UiEvent::TimelineInvalidated { conv: C2 });
    src.search_due();
    answer_all(&mut src, &sent, &store, &db);
    assert_eq!(shown(&src), vec![MessageId(100_003)]);
    // A clear of the other chat comes as a change of the chat's row.
    db.hits.clear();
    src.on_event(&UiEvent::ConversationsChanged { ids: vec![C1] });
    src.search_due();
    answer_all(&mut src, &sent, &store, &db);
    assert!(shown(&src).is_empty());
}

/// S-P and H5: a chat that turns protected drops its hits at once, on the list's answer and
/// before any search answer; lifted, they show again. A chat the list does not hold shows none.
#[test]
fn a_chat_that_turns_protected_drops_its_search_hits_at_once() {
    let (mut src, sent, mut store, _) = on_screen();
    store.protected = vec![1];
    src.on_event(&UiEvent::ConversationsChanged { ids: vec![C1] });
    let (ch, held) = answer_but_searches(&mut src, &sent, &store);
    assert!(
        held.is_empty(),
        "no search was asked yet (the read again waits)"
    );
    assert!(ch.search, "the list's answer tells the front-end");
    assert_eq!(shown(&src), vec![MessageId(200_004)]);
    store.protected.clear();
    src.on_event(&UiEvent::ConversationsChanged { ids: vec![C1] });
    answer_but_searches(&mut src, &sent, &store);
    assert_eq!(shown(&src), vec![MessageId(200_004), MessageId(100_003)]);
    // A hit in a chat the list does not hold (hidden, not read yet) shows nothing.
    let db = Db::new(vec![hit(9, 1, "plan"), hit(2, 4, "the plan")]);
    src.search_messages("the plan", None);
    answer_all(&mut src, &sent, &store, &db);
    src.search_messages("plan", None);
    answer_all(&mut src, &sent, &store, &db);
    assert_eq!(shown(&src), vec![MessageId(200_004)]);
}

/// A chat removed and an account removed take their hits off at once.
#[test]
fn a_removed_chat_and_a_removed_account_take_their_search_hits_off_at_once() {
    let (mut src, sent, store, _) = on_screen();
    let ch = src.on_event(&UiEvent::ConversationsRemoved { ids: vec![C2] });
    assert!(ch.search);
    assert_eq!(shown(&src), vec![MessageId(100_003)]);
    let ch = src.on_event(&UiEvent::AccountRemoved {
        account: AccountId(1),
    });
    assert!(ch.search);
    assert!(shown(&src).is_empty());
    let _ = answer_but_searches(&mut src, &sent, &store);
}

/// Takes the one search query sent so far (the others stay unanswered).
fn held_search(sent: &Sent) -> (QueryToken, SearchQuery) {
    let mut found = None;
    for (t, q) in take_queries(sent) {
        if let Query::Search(s) = q {
            assert!(found.is_none(), "one search in flight");
            found = Some((t, s));
        }
    }
    found.expect("a search in flight")
}

/// Privacy LOW-5: an answer on its way when a chat or an account is removed was read before
/// the removal: it is dropped when it comes (the list still holds the chat until its own read),
/// and the hits are read again after the removal.
#[test]
fn an_answer_read_before_a_removal_brings_none_of_its_hits_back() {
    let (mut src, sent, store, db) = on_screen();
    // A read again of the hits is on its way when the chat goes.
    src.on_event(&UiEvent::MessagesChanged {
        conv: C2,
        ids: Vec::new(),
    });
    src.search_due();
    let (t, q) = held_search(&sent);
    src.on_event(&UiEvent::ConversationsRemoved { ids: vec![C2] });
    assert_eq!(shown(&src), vec![MessageId(100_003)]);
    src.on_event(&result(t, QueryData::Search(db.page(&q))));
    assert_eq!(
        shown(&src),
        vec![MessageId(100_003)],
        "the old answer is dropped"
    );
    // The read after the removal: the core no longer finds the chat's hit.
    let mut after = db.clone();
    after.hits.retain(|h| h.conv != C2);
    let (t, q) = held_search(&sent);
    assert_eq!((q.before, q.limit), (None, SEARCH_PAGE));
    src.on_event(&result(t, QueryData::Search(after.page(&q))));
    assert_eq!(shown(&src), vec![MessageId(100_003)]);
    // The same for an account.
    src.on_event(&UiEvent::MessagesChanged {
        conv: C1,
        ids: Vec::new(),
    });
    src.search_due();
    let (t, q) = held_search(&sent);
    src.on_event(&UiEvent::AccountRemoved {
        account: AccountId(1),
    });
    src.on_event(&result(t, QueryData::Search(after.page(&q))));
    assert!(shown(&src).is_empty(), "the old answer is dropped");
    let _ = answer_but_searches(&mut src, &sent, &store);
}

/// A store whose chat 1 has 200 unread messages from the other person (and some of ours).
fn unread_store() -> Store {
    let mut store = Store::new(&[(1, 0), (2, 5)]);
    store
        .convs
        .insert(1, (1..=200).map(|n| message(1, n, true)).collect());
    store
}

/// The timeline queries sent so far, in order (the others stay).
fn timeline_anchors(sent: &Sent) -> Vec<(QueryToken, TimelineQuery)> {
    let mut out = Vec::new();
    for (t, q) in take_queries(sent) {
        if let Query::Timeline(tq) = q {
            out.push((t, tq));
        }
    }
    out
}

/// B1: opening an old hit asks the window AROUND it and no page at the newest messages first,
/// the answer jumps to it, and nothing is marked read although the window is focused, the chat
/// visible and its messages unread (the window ends before the newest messages).
#[test]
fn open_at_an_old_search_hit_reads_around_it_and_marks_nothing_read() {
    let store = unread_store();
    let (mut src, sent) = opened(&store);
    src.set_presence(true, true);
    let hit = MessageId(100_020);
    let ch = src.open_at(C1, hit);
    assert!(ch.timeline && ch.header);
    let qs = take_queries(&sent);
    let timelines: Vec<&TimelineQuery> = qs
        .iter()
        .filter_map(|(_, q)| match q {
            Query::Timeline(tq) => Some(tq),
            _ => None,
        })
        .collect();
    assert_eq!(timelines.len(), 1, "one timeline read: around the hit");
    assert_eq!(timelines[0].anchor, TimelineAnchor::Around(hit));
    let mut ch = Changes::default();
    for (t, q) in qs {
        ch.merge(src.on_event(&result(t, store.answer(&q))));
    }
    let (more, _) = answer_all(&mut src, &sent, &store, &Db::default());
    ch.merge(more);
    assert_eq!(ch.jump, Some(hit));
    assert!(src.paging().has_newer);
    assert!(src.timeline(C1).iter().any(|r| r.message_id() == Some(hit)));
    assert_eq!(mark_reads(&sent), 0, "an old hit marks nothing read");
}

/// Privacy HIGH-1: a window opened at an old hit re-reads its own stretch when the chat
/// changes. Five changes in a row (each answered) leave it where it was, away from the newest
/// messages, and nothing is marked read (a reload with room for 50 more rows walked it to the
/// newest in three and marked the chat read).
#[test]
fn reloads_of_a_window_at_an_old_hit_keep_it_there_and_mark_nothing_read() {
    let store = unread_store();
    let (mut src, sent) = opened(&store);
    src.set_presence(true, true);
    let hit = MessageId(100_020);
    src.open_at(C1, hit);
    answer_all(&mut src, &sent, &store, &Db::default());
    let len = src.window_len();
    assert!(src.paging().has_newer, "the window ends before the newest");
    for _ in 0..5 {
        src.on_event(&UiEvent::MessagesChanged {
            conv: C1,
            ids: Vec::new(),
        });
        answer_all(&mut src, &sent, &store, &Db::default());
        assert_eq!(src.window_len(), len, "the window keeps its stretch");
    }
    assert!(src.paging().has_newer);
    assert!(src.timeline(C1).iter().any(|r| r.message_id() == Some(hit)));
    assert_eq!(mark_reads(&sent), 0, "an old hit's chat is not marked read");
}

/// B1: a hit at the newest messages reads as opening the chat does: it is marked read.
#[test]
fn open_at_a_search_hit_at_the_newest_messages_marks_the_chat_read() {
    let store = unread_store();
    let (mut src, sent) = opened(&store);
    src.set_presence(true, true);
    let ch = src.open_at(C1, MessageId(100_199));
    assert!(ch.timeline);
    let (ch, _) = answer_all(&mut src, &sent, &store, &Db::default());
    assert_eq!(ch.jump, Some(MessageId(100_199)));
    assert!(!src.paging().has_newer);
    assert_eq!(mark_reads(&sent), 1, "as any open at the newest messages");
}

/// L1: on the open chat a loaded row is shown at once (no query); one not loaded moves the
/// window around it.
#[test]
fn open_at_on_the_open_chat_jumps_at_once_or_moves_the_window() {
    let store = unread_store();
    let (mut src, sent) = opened(&store);
    src.open_conversation(Some(C1));
    answer_all(&mut src, &sent, &store, &Db::default());
    let loaded = MessageId(100_180);
    let ch = src.open_at(C1, loaded);
    assert_eq!(ch.jump, Some(loaded));
    assert!(take_queries(&sent).is_empty(), "a loaded row asks nothing");
    let old = MessageId(100_020);
    assert_eq!(src.open_at(C1, old).jump, None);
    let asked = timeline_anchors(&sent);
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0].1.anchor, TimelineAnchor::Around(old));
    let ch = src.on_event(&result(
        asked[0].0,
        store.answer(&Query::Timeline(asked[0].1.clone())),
    ));
    assert_eq!(ch.jump, Some(old));
}

/// B1 kept by this side too: a hit opened right after its chat (the page at the newest messages
/// still on its way) takes that page's place: its answer is dropped and marks nothing read.
#[test]
fn open_at_right_after_an_open_drops_the_page_at_the_newest() {
    let store = unread_store();
    let (mut src, sent) = opened(&store);
    src.set_presence(true, true);
    src.open_conversation(Some(C1));
    let first = take_queries(&sent);
    assert!(first.iter().any(|(_, q)| matches!(
        q,
        Query::Timeline(TimelineQuery {
            anchor: TimelineAnchor::Newest,
            ..
        })
    )));
    let hit = MessageId(100_020);
    src.open_at(C1, hit);
    let around = timeline_anchors(&sent);
    assert_eq!(
        around.len(),
        1,
        "asked at once, the newest page still in flight"
    );
    assert_eq!(around[0].1.anchor, TimelineAnchor::Around(hit));
    for (t, q) in first {
        let ch = src.on_event(&result(t, store.answer(&q)));
        assert!(ch.jump.is_none());
    }
    assert_eq!(mark_reads(&sent), 0, "the newest page was dropped");
    let ch = src.on_event(&result(
        around[0].0,
        store.answer(&Query::Timeline(around[0].1.clone())),
    ));
    assert_eq!(ch.jump, Some(hit));
    answer_all(&mut src, &sent, &store, &Db::default());
    assert_eq!(mark_reads(&sent), 0);
}

/// A hit that went away meanwhile: search's own notice, then the chat opens at its newest
/// messages and does what any open does (here: it is marked read).
#[test]
fn open_at_a_search_hit_that_went_away_says_so_and_opens_at_the_newest() {
    let store = unread_store();
    let (mut src, sent) = opened(&store);
    src.set_presence(true, true);
    src.open_at(C1, MessageId(100_999));
    let first = timeline_anchors(&sent);
    assert_eq!(
        first[0].1.anchor,
        TimelineAnchor::Around(MessageId(100_999))
    );
    let ch = src.on_event(&result(
        first[0].0,
        store.answer(&Query::Timeline(first[0].1.clone())),
    ));
    assert!(ch.notice);
    assert_eq!(src.notice(), "The message is no longer available.");
    let next = timeline_anchors(&sent);
    assert_eq!(next.len(), 1);
    assert_eq!(next[0].1.anchor, TimelineAnchor::Newest);
    src.on_event(&result(
        next[0].0,
        store.answer(&Query::Timeline(next[0].1.clone())),
    ));
    answer_all(&mut src, &sent, &store, &Db::default());
    assert_eq!(mark_reads(&sent), 1);
    // A quote whose original went away keeps its own words.
    src.clear_notice();
    src.jump_to(MessageId(100_998));
    let quote = timeline_anchors(&sent);
    src.on_event(&result(
        quote[0].0,
        store.answer(&Query::Timeline(quote[0].1.clone())),
    ));
    assert_eq!(src.notice(), "The original message is no longer available.");
}

/// The fixture answers a search from its own messages and says what it is told of the index;
/// the empty source has no search.
#[test]
fn the_fixture_answers_a_search_and_the_empty_source_has_none() {
    let mut fx = FixtureSource::new().expect("fixture");
    assert!(!fx.search_view(&ConversationFilter::default()).active);
    assert!(fx.search_messages("satur", None).search);
    let view = fx.search_view(&ConversationFilter::default());
    assert!(view.active && !view.loading && view.index_ready);
    assert!(view.rows.len() >= 2, "Saturday is in several chats");
    assert!(
        view.rows
            .iter()
            .all(|r| r.snippet.html.contains("<b>Saturday</b>"))
    );
    assert!(
        view.rows.windows(2).all(|w| w[0].sort_key >= w[1].sort_key),
        "newest first"
    );
    let one = view.rows[0].conv;
    fx.search_messages("satur", Some(one));
    let view = fx.search_view(&ConversationFilter::default());
    assert!(view.rows.iter().all(|r| r.conv == one));
    fx.set_search_index(false, 40);
    let view = fx.search_view(&ConversationFilter::default());
    assert_eq!((view.index_ready, view.index_percent), (false, 40));
    // Opening a hit names it once its chat's rows hold it.
    let row = view.rows[0].clone();
    assert_eq!(fx.open_at(row.conv, row.message).jump, Some(row.message));
    assert!(fx.close_search().search);
    assert!(!fx.search_view(&ConversationFilter::default()).active);
    let mut empty = EmptySource::new(NOW);
    assert!(!empty.search_messages("plan", None).search);
    assert!(!empty.search_view(&ConversationFilter::default()).active);
    assert!(!empty.more_search());
}

/// 150 hits of "plan" in chat 1, newest has the highest number.
fn many() -> (Store, Db) {
    (
        Store::new(&[(1, 10), (2, 5)]),
        Db::new(
            (1..=150)
                .map(|n| hit(1, n, &format!("the plan {n}")))
                .collect(),
        ),
    )
}

/// Answers the one search in flight from `db`; returns what it asked.
fn answer_one(src: &mut CoreSource, sent: &Sent, db: &Db) -> SearchQuery {
    let (t, q) = held_search(sent);
    src.on_event(&result(t, QueryData::Search(db.page(&q))));
    q
}

/// F5: a re-read with 150 hits loaded reads into a buffer; the shown rows stay while it reads
/// and are the same 150 after it (the poll of a filling index no longer cuts them to a page).
#[test]
fn a_re_read_keeps_all_loaded_hits_and_swaps_them_when_the_buffer_is_whole() {
    let (store, mut db) = many();
    let (mut src, sent) = opened(&store);
    src.search.debounce = Duration::ZERO;
    src.search.index_wait = Duration::ZERO;
    db.index = Some(SearchIndexState {
        ready: false,
        percent: 10,
    });
    src.search_messages("plan", None);
    answer_all(&mut src, &sent, &store, &db);
    assert!(src.more_search());
    answer_all(&mut src, &sent, &store, &db);
    assert!(src.more_search());
    answer_all(&mut src, &sent, &store, &db);
    let before = shown(&src);
    assert_eq!(before.len(), 150);
    // The poll: page 1 of 100, then 50; the rows stay in between.
    src.search_due();
    let q = answer_one(&mut src, &sent, &db);
    assert_eq!((q.before, q.limit), (None, 100));
    assert_eq!(shown(&src), before, "the shown hits stay while it reads");
    let q = answer_one(&mut src, &sent, &db);
    assert!(q.before.is_some());
    assert_eq!(q.limit, 50);
    assert_eq!(shown(&src), before, "and after it: the same 150");
    // The poll goes on while the index fills, each time the same.
    src.search_due();
    answer_all(&mut src, &sent, &store, &db);
    assert_eq!(shown(&src), before);
    assert!(!src.search_view(&ConversationFilter::default()).more);
}

/// F5 (privacy): a hit deleted beyond page 1 leaves on the re-read of its chat (the rule of
/// G1m, now for row 130), and nothing of the old tail stays.
#[test]
fn a_hit_deleted_beyond_the_first_page_leaves_on_the_re_read() {
    let (store, mut db) = many();
    let (mut src, sent) = opened(&store);
    src.search.debounce = Duration::ZERO;
    src.search_messages("plan", None);
    answer_all(&mut src, &sent, &store, &db);
    for _ in 0..2 {
        assert!(src.more_search());
        answer_all(&mut src, &sent, &store, &db);
    }
    assert_eq!(shown(&src).len(), 150);
    // Row 130 of the newest-first list is message 150 - 129 = 21.
    let doomed = MessageId(100_021);
    assert_eq!(shown(&src)[129], doomed);
    db.hits.retain(|h| h.message != doomed);
    src.on_event(&UiEvent::MessagesChanged {
        conv: C1,
        ids: vec![doomed],
    });
    assert!(
        src.search_deadline()
            .is_some_and(|at| at <= Instant::now() + Duration::from_millis(1500)),
        "it is read again within 1.5 s"
    );
    src.search_due();
    answer_all(&mut src, &sent, &store, &db);
    let now = shown(&src);
    assert_eq!(now.len(), 149);
    assert!(!now.contains(&doomed));
}

/// F5: the poll of a filling index backs off: 2 s, doubling, at most 30 s.
#[test]
fn the_index_poll_backs_off_to_thirty_seconds() {
    use crate::search::{INDEX_REREAD_AFTER, INDEX_REREAD_MAX, index_poll_delay};
    let seq: Vec<u64> = (0..7)
        .map(|i| index_poll_delay(INDEX_REREAD_AFTER, i).as_secs())
        .collect();
    assert_eq!(seq, [2, 4, 8, 16, 30, 30, 30]);
    assert_eq!(INDEX_REREAD_MAX, Duration::from_secs(30));
    assert_eq!(index_poll_delay(Duration::ZERO, 9), Duration::ZERO);
    // Through the source: each answer with a filling index arms a longer wait; a new text
    // starts at two seconds again.
    let (store, mut db) = world();
    let (mut src, sent) = opened(&store);
    db.index = Some(SearchIndexState {
        ready: false,
        percent: 5,
    });
    src.search_messages("plan", None);
    answer_all(&mut src, &sent, &store, &db);
    let first = src.search_deadline().expect("armed") - Instant::now();
    assert!(first <= Duration::from_secs(2));
    src.reread_search();
    answer_all(&mut src, &sent, &store, &db);
    let second = src.search_deadline().expect("armed") - Instant::now();
    assert!(second > Duration::from_secs(3) && second <= Duration::from_secs(4));
    src.search_messages("plan is", None);
    answer_all(&mut src, &sent, &store, &db);
    let again = src.search_deadline().expect("armed") - Instant::now();
    assert!(again <= Duration::from_secs(2), "a new text resets it");
}

/// F6: a new text of the same scope keeps the shown hits until its first answer; a change of
/// scope or a cleared text clears at once; "answered" is for the current text only.
#[test]
fn a_refinement_keeps_the_rows_until_its_answer() {
    let (store, db) = world();
    let (mut src, sent) = opened(&store);
    src.search_messages("plan", None);
    answer_all(&mut src, &sent, &store, &db);
    let rows = shown(&src);
    assert_eq!(rows.len(), 2);
    src.search_messages("plan is", None);
    let view = src.search_view(&ConversationFilter::default());
    assert_eq!(shown(&src), rows, "kept while the new text is asked");
    assert!(!view.answered && !view.more && view.loading);
    answer_all(&mut src, &sent, &store, &db);
    assert_eq!(shown(&src), vec![MessageId(200_004)]);
    assert!(src.search_view(&ConversationFilter::default()).answered);
    // A change of scope clears at once.
    src.search_messages("plan", Some(C1));
    assert!(shown(&src).is_empty());
    answer_all(&mut src, &sent, &store, &db);
    assert_eq!(shown(&src), vec![MessageId(100_003)]);
    // So does a cleared text.
    src.close_search();
    assert!(shown(&src).is_empty());
}

/// F6: a search that ended capped with no hits can be asked on: another round of chained pages.
#[test]
fn a_capped_search_with_no_hits_asks_a_further_round() {
    use crate::search::MAX_CHAINED_PAGES;
    let (store, _) = world();
    let (mut src, sent) = opened(&store);
    let stalled = Db {
        stall: true,
        ..Db::default()
    };
    src.search_messages("plan", None);
    let (_, asked) = answer_all(&mut src, &sent, &store, &stalled);
    assert_eq!(asked.len() as u32, 1 + MAX_CHAINED_PAGES);
    let view = src.search_view(&ConversationFilter::default());
    assert!(view.answered && view.capped && view.rows.is_empty());
    assert!(src.more_search(), "Search Further asks on");
    let (_, asked) = answer_all(&mut src, &sent, &store, &stalled);
    assert_eq!(asked.len() as u32, 1 + MAX_CHAINED_PAGES);
}

/// A group member's hit shows the picture of that member's 1:1 chat (the timeline's rule); a
/// member without such a chat shows initials (an empty URL).
#[test]
fn a_group_hit_shows_the_picture_of_its_senders_direct_chat() {
    use kourier_model::{AccountUuid, BlobKey, BlobRef, ConvKind};

    struct Urls;
    impl AvatarUrls for Urls {
        fn url(&self, _account: &AccountUuid, avatar: &BlobRef) -> Option<String> {
            Some(format!("image://kourier/avatar/{}", avatar.path.display()))
        }
    }

    let store = Store::new(&[(1, 10), (2, 5)]);
    let mut other = hit(1, 2, "a plan of someone else");
    other.author = Some(user(2));
    let db = Db::new(vec![hit(1, 3, "bring the plan"), other]);
    let mut direct = super::tests::conv_row(3, "Ari", 1);
    direct.kind = ConvKind::Direct;
    direct.peer = Some(user(1));
    direct.avatar = Some(BlobRef {
        path: "7".into(),
        key: BlobKey::new([1; 32]),
    });
    let answer = |q: &Query| match q {
        Query::Search(s) => QueryData::Search(db.page(s)),
        Query::Conversations(_) => {
            let mut data = store.answer(q);
            if let QueryData::Conversations(page) = &mut data {
                page.rows.push(direct.clone());
            }
            data
        }
        q => store.answer(q),
    };
    let (mut src, sent) = source();
    src.set_avatar_urls(Box::new(Urls));
    let settle = |src: &mut CoreSource| {
        for _ in 0..100 {
            let qs = take_queries(&sent);
            if qs.is_empty() {
                return;
            }
            for (t, q) in qs {
                src.on_event(&result(t, answer(&q)));
            }
        }
        panic!("queries do not settle");
    };
    src.reload();
    settle(&mut src);
    src.search_messages("plan", None);
    settle(&mut src);
    let view = src.search_view(&ConversationFilter::default());
    let pictures: Vec<(MessageId, &str)> = view
        .rows
        .iter()
        .map(|r| (r.message, r.sender_avatar_url.as_str()))
        .collect();
    assert_eq!(
        pictures,
        vec![
            (MessageId(100_003), "image://kourier/avatar/7"),
            (MessageId(100_002), ""),
        ]
    );
}

/// RegExp search (R5): the toggles set the mode a search asks in, the same text in another mode is
/// another search, the page's compiled pattern is what the chat titles are filtered by, and a
/// switch back to words drops it.
#[test]
fn a_pattern_search_asks_in_its_mode_and_filters_the_titles_with_the_pages_pattern() {
    let (store, db) = world();
    let (mut src, sent) = opened(&store);
    assert!(src.search_pattern().is_none());
    // The mode alone starts no search.
    assert!(!UiSource::set_search_mode(&mut src, true, false).search);
    assert!(take_queries(&sent).is_empty());

    src.search_messages("c2 v", None);
    let (_, asked) = answer_all(&mut src, &sent, &store, &db);
    assert_eq!(asked[0].mode, SearchMode::Pattern { match_case: false });
    assert!(src.search_pattern().is_some(), "the page's pattern is kept");
    // Titles pass by the pattern, not by the text: "c2 v" is c2's title.
    let titles = |src: &CoreSource| {
        let filter = ConversationFilter {
            query: "c2 v".to_owned(),
            regex: true,
            pattern: src.search_pattern(),
            ..ConversationFilter::default()
        };
        src.conversations(&filter)
            .iter()
            .map(|r| r.title.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(titles(&src), vec!["c2 v0".to_owned()]);
    // Before the first answer there is no pattern, and no title passes.
    let none = ConversationFilter {
        query: "c2 v".to_owned(),
        regex: true,
        ..ConversationFilter::default()
    };
    assert!(src.conversations(&none).is_empty());

    // The same text, another mode: a new search in that mode.
    assert!(UiSource::set_search_mode(&mut src, true, true).search);
    let (_, asked) = answer_all(&mut src, &sent, &store, &db);
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0].mode, SearchMode::Pattern { match_case: true });
    assert!(
        !src.search_messages("c2 v", None).search,
        "same mode, same text"
    );
    // Back to words: the pattern goes and the next search asks for words.
    assert!(UiSource::set_search_mode(&mut src, false, false).search);
    let (_, asked) = answer_all(&mut src, &sent, &store, &db);
    assert_eq!(asked[0].mode, SearchMode::Words);
    assert!(src.search_pattern().is_none());
}
