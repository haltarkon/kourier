//! The pure half of the query (the MATCH a typed text gives) and the search connections'
//! interrupts (`Store::search`): a newer search ends the running one, a queued one that a newer
//! one passed ends unanswered (MEDIUM-3), the time cap ends a step that runs on, nothing is
//! opened before the file is judged, and the connections open again after a release.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use kourier_db::DbKey;
use kourier_db::rusqlite::Connection;
use kourier_model::ConversationId;

use super::*;
use crate::error::CoreError;
use crate::store::{Searched, Store};

fn expr(text: &str, chats: Option<&[ConversationId]>) -> Option<String> {
    match_expr(&query_terms(text), chats)
}

#[test]
fn the_match_holds_quoted_terms_and_the_scope_in_one_expression() {
    let a = ConversationId(12);
    let b = ConversationId(34);
    for (text, chats, want) in [
        ("файли", None, Some(r#"words:("файли"*)"#)),
        // One letter matches the whole word only; two and more are prefixes (MIN_PREFIX).
        ("a", None, Some(r#"words:("a")"#)),
        ("ok go", None, Some(r#"words:("ok"* AND "go"*)"#)),
        (
            "Hello, World",
            Some(&[a, b][..]),
            Some(r#"words:("hello"* AND "world"*) AND meta:("c12" OR "c34")"#),
        ),
        // His `"`, `*`, `:`, `(` and the operators are nothing or plain words.
        (
            r#"NEAR(x "y" OR z*)"#,
            None,
            Some(r#"words:("near"* AND "x" AND "y" AND "or"* AND "z")"#),
        ),
        ("meta:c1", None, Some(r#"words:("meta"* AND "c1"*)"#)),
        // No word: nothing can match.
        ("\"*:({}) -- \u{0}", None, None),
        ("", None, None),
        // An empty list of chats is no chat at all, never every chat (M11).
        ("hello", Some(&[][..]), None),
    ] {
        assert_eq!(expr(text, chats).as_deref(), want, "{text:?}");
    }
}

#[test]
fn a_long_text_is_cut_and_never_more_than_the_term_limit() {
    let long = "слово ".repeat(20_000);
    let e = expr(&long, None).expect("words");
    assert_eq!(
        e.matches('"').count(),
        2 * crate::search::normalize::MAX_QUERY_TERMS
    );
}

/// A store whose search.db is made and judged (what the search task does at `UiReady`).
fn store() -> (tempfile::TempDir, Arc<Store>) {
    let (dir, store) = unjudged();
    let link = store.search_link();
    crate::search::index::Files::open(link.paths()).expect("search.db");
    link.set_ready(true);
    (dir, store)
}

/// A store whose search task has not judged search.db yet.
fn unjudged() -> (tempfile::TempDir, Arc<Store>) {
    let dir = tempfile::tempdir().expect("tempdir");
    let (store, _) = Store::open(&dir.path().join("core.db"), &DbKey::from([9; 32])).expect("open");
    (dir, Arc::new(store))
}

/// The page a search gave; panics on the other answers.
fn done<R: std::fmt::Debug>(r: crate::error::Result<Searched<R>>) -> crate::error::Result<R> {
    match r? {
        Searched::Done(r) => Ok(r),
        other => panic!("the search did not run: {other:?}"),
    }
}

/// A statement that never ends by itself.
fn forever(c: &Connection) -> crate::error::Result<i64> {
    Ok(c.query_row(
        "WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM n) SELECT count(*) FROM n",
        [],
        |r| r.get(0),
    )?)
}

fn is_interrupt(r: &crate::error::Result<i64>) -> bool {
    matches!(r, Err(CoreError::Sql(e)) if interrupted(e))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_newer_search_ends_the_running_one_at_once() {
    let (_dir, store) = store();
    let (began, began_rx) = std::sync::mpsc::channel();
    let first = {
        let store = store.clone();
        tokio::spawn(async move {
            done(
                store
                    .search(move |c, _, _| {
                        let _ = began.send(Instant::now());
                        forever(c)
                    })
                    .await,
            )
        })
    };
    let began = tokio::task::spawn_blocking(move || began_rx.recv().expect("began"))
        .await
        .expect("join");
    let asked = Instant::now();
    let second = store.search(|c, _, _| Ok(c.query_row("SELECT 7", [], |r| r.get::<_, i64>(0))?));
    let second = done(second.await);
    let first = first.await.expect("join");
    assert!(is_interrupt(&first), "the older search was interrupted");
    assert_eq!(second.expect("the newer search runs"), 7);
    // Long before the time cap would have ended it.
    assert!(
        asked.elapsed() < TIME_CAP,
        "ended {:?} after the newer search came (began {:?} before)",
        asked.elapsed(),
        asked - began
    );
}

/// The race of the full ci of 2026-11-14 made certain: the newer search comes after the older
/// one took the connections but before its first statement, where SQLite drops an interrupt
/// (`sqlite3_interrupt` with no statement running is a no-op). The newer search says it again
/// while it waits, so the older one ends at once once its statement runs, not at the time cap.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_newer_search_ends_one_that_has_not_begun_its_statement() {
    let (_dir, store) = store();
    let (began, began_rx) = std::sync::mpsc::channel();
    let first = {
        let store = store.clone();
        tokio::spawn(async move {
            done(
                store
                    .search(move |c, _, _| {
                        let _ = began.send(Instant::now());
                        // No statement runs yet when the newer search's first interrupt comes.
                        std::thread::sleep(Duration::from_millis(200));
                        forever(c)
                    })
                    .await,
            )
        })
    };
    let _ = tokio::task::spawn_blocking(move || began_rx.recv().expect("began"))
        .await
        .expect("join");
    let asked = Instant::now();
    let second = store.search(|c, _, _| Ok(c.query_row("SELECT 7", [], |r| r.get::<_, i64>(0))?));
    let second = done(second.await);
    let first = first.await.expect("join");
    assert!(is_interrupt(&first), "the older search was interrupted");
    assert_eq!(second.expect("the newer search runs"), 7);
    // The older one's pause (200 ms) and a few tries, long before the cap and its margin (1.25 s).
    let took = asked.elapsed();
    assert!(
        took < Duration::from_millis(700),
        "ended {took:?} after the newer search came"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_time_cap_ends_a_step_that_runs_on() {
    let (_dir, store) = store();
    let t = Instant::now();
    let out = done(store.search(|c, _, _| forever(c)).await);
    assert!(is_interrupt(&out));
    assert!(
        t.elapsed() >= TIME_CAP,
        "not before the cap: {:?}",
        t.elapsed()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_search_connection_opens_again_after_it_was_released() {
    let (_dir, store) = store();
    let one = |c: &Connection, core: &Connection, _: Instant| {
        Ok(c.query_row("SELECT 1", [], |r| r.get::<_, i64>(0))?
            + core.query_row("SELECT 1", [], |r| r.get::<_, i64>(0))?)
    };
    assert_eq!(done(store.search(one).await).expect("first"), 2);
    store.release_memory().await.expect("release");
    assert!(!store.search_is_open().await, "released");
    assert_eq!(done(store.search(one).await).expect("again"), 2);
}

/// MEDIUM-3: a search that waited for the connections while a newer one was asked ends
/// unanswered; only the newest runs after the one that held them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_queued_search_that_a_newer_one_passed_ends_unanswered() {
    let (_dir, store) = store();
    let (hold, held) = std::sync::mpsc::channel::<()>();
    let (began, began_rx) = std::sync::mpsc::channel();
    // The first holds the connections and does not stop for an interrupt.
    let first = {
        let store = store.clone();
        tokio::spawn(async move {
            store
                .search(move |_, _, _| {
                    let _ = began.send(());
                    let _ = held.recv();
                    Ok(1_i64)
                })
                .await
        })
    };
    tokio::task::spawn_blocking(move || began_rx.recv().expect("began"))
        .await
        .expect("join");
    let ran = Arc::new(AtomicUsize::new(0));
    let asked = |store: Arc<Store>, ran: Arc<AtomicUsize>, n: i64| {
        tokio::spawn(async move {
            store
                .search(move |_, _, _| {
                    ran.fetch_add(1, Ordering::SeqCst);
                    Ok(n)
                })
                .await
        })
    };
    let second = asked(store.clone(), ran.clone(), 2);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let third = asked(store.clone(), ran.clone(), 3);
    tokio::time::sleep(Duration::from_millis(100)).await;
    drop(hold);
    assert!(matches!(
        first.await.expect("join").expect("first"),
        Searched::Done(1)
    ));
    assert!(
        matches!(
            second.await.expect("join").expect("second"),
            Searched::Superseded
        ),
        "the passed search ends unanswered"
    );
    assert!(matches!(
        third.await.expect("join").expect("third"),
        Searched::Done(3)
    ));
    assert_eq!(ran.load(Ordering::SeqCst), 1, "only the newest one ran");
}

/// Nothing is opened before the search task judged search.db: a search waits for it, at most its
/// time cap, and answers "not ready" (never "nothing"); once judged, it runs, woken by the judge.
///
/// On tokio's PAUSED clock: the cap passes in no real time, and a search that slept out its cap
/// instead of being woken shows a whole cap on that clock. STRIKE 1 (2026-11-11, `ci --quick` on
/// the server) was the real-time form: a 100 ms sleep and the open of search.db after it took the
/// rest of the 1 s cap under load, and the search answered "not ready". That form also passed
/// when the judge woke nobody (the search found the flag after its cap).
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_search_before_the_file_is_judged_answers_not_ready() {
    let (_dir, store) = unjudged();
    let one = |c: &Connection, _: &Connection, _: Instant| {
        Ok(c.query_row("SELECT 1", [], |r| r.get::<_, i64>(0))?)
    };
    let t = tokio::time::Instant::now();
    assert!(matches!(
        store.search(one).await.expect("not ready"),
        Searched::NotReady
    ));
    assert!(t.elapsed() >= TIME_CAP, "it waited for the open");
    assert!(!store.search_is_open().await, "nothing was opened");
    // The judge comes while a search waits: it runs. The file is made BEFORE the search is
    // asked, and the flag is set as soon as the search is seen waiting for it.
    let link = store.search_link();
    crate::search::index::Files::open(link.paths()).expect("search.db");
    let before = link.searches_waiting();
    let waiting = {
        let store = store.clone();
        tokio::spawn(async move { store.search(one).await })
    };
    let asked = Instant::now();
    while link.searches_waiting() == before {
        assert!(
            asked.elapsed() < Duration::from_secs(60),
            "the search never waited for the judge"
        );
        tokio::task::yield_now().await;
    }
    let judged = tokio::time::Instant::now();
    link.set_ready(true);
    assert!(matches!(
        waiting.await.expect("join").expect("judged"),
        Searched::Done(1)
    ));
    assert!(
        judged.elapsed() < TIME_CAP,
        "the judge did not wake the waiting search: it slept out its cap"
    );
}

/// Privacy LOW-3: the builder's quotes are a second guard, not the only one. Whatever he types,
/// every term is an FTS5 bareword of `ascii` (ASCII letters lowered, digits, or bytes above
/// 127): no separator (a `.`, a `'`), no `"`, `*`, `:`, `(`, `^`, `+`, `-` or `{` reaches the
/// expression, and FTS5's keywords (upper case) never do. So the MATCH, quoted or not, parses,
/// and a small index of search.db's own schema answers it without an error (a term holding a
/// separator would make a quoted phrase, which `detail=column` refuses).
mod barewords {
    use proptest::prelude::*;

    use super::*;

    fn index() -> Connection {
        let c = Connection::open_in_memory().expect("memory");
        c.execute_batch(crate::search::file::SCHEMA)
            .expect("schema");
        c.execute(
            "INSERT INTO messages_fts(rowid, words, meta) VALUES (1, 'plain words', 'c1')",
            [],
        )
        .expect("row");
        c
    }

    fn typed() -> impl Strategy<Value = String> {
        prop_oneof![
            any::<String>(),
            "[ -~]{0,40}",
            "[a-zA-Z0-9 .'\"*:(){}^+-]{0,30}",
            "(AND|OR|NOT|NEAR|and|or|3\\.14|it's|e-mail|c1|meta:|words:| ){0,8}",
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(400))]

        #[test]
        fn every_term_is_a_bareword_and_the_match_parses(text in typed()) {
            let terms = query_terms(&text);
            for t in &terms {
                prop_assert!(!t.text.is_empty());
                prop_assert!(
                    t.text
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || !c.is_ascii()),
                    "{:?}",
                    t.text
                );
            }
            let c = index();
            for chats in [None, Some(&[ConversationId(1), ConversationId(2)][..])] {
                let Some(e) = match_expr(&terms, chats) else {
                    prop_assert!(terms.is_empty());
                    continue;
                };
                let found = c.query_row(
                    "SELECT count(*) FROM messages_fts WHERE messages_fts MATCH ?1",
                    [&e],
                    |r| r.get::<_, i64>(0),
                );
                prop_assert!(found.is_ok(), "{:?}", found.err().map(|e| e.to_string()));
            }
        }
    }
}
