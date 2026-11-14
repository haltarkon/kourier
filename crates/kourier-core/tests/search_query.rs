//! The search itself (G1e): `Query::Search` walks search.db's index and looks each id up in
//! core.db. Hostile text through the builder, the query-time rules, the scope inside the MATCH
//! (a counter of the rows looked up), short terms, stale rows and scattered ideographs, pages
//! against a brute-force sort, an hour over the cap (H4), a cursor across inserts, the plans
//! (each statement on its own file), and the running core on its read connections.
//! Synthetic text only; each test word is one no other row holds.

use crate::support;

use kourier_backend_api::BackendEvent;
use kourier_core::facade::{MAX_SEARCH_PAGE, Query, QueryData, SearchPage, SearchQuery, UiCommand};
use kourier_core::testing::{IngestHarness, search};
use kourier_fake::scripts::{T0, remote};
use kourier_fake::{Flavor, Script};
use kourier_model::{
    Body, ConvKey, ConversationId, MessageId, MsgIdScope, NetworkKind, Redacted, Text, UserId,
};
use proptest::prelude::*;

use support::{Env, WAIT, add_linked, eventually};

const ME: &str = "aci:00000000-0000-4000-8000-0000000000aa";
const ALICE: &str = "aci:00000000-0000-4000-8000-0000000a11ce";
const CAROL: &str = "aci:00000000-0000-4000-8000-0000000ca202";

/// One hour of sort keys (`sort_key` is milliseconds times 1024): the rowid's bucket.
const HOUR: i64 = 1024 * 3_600_000;

fn uid(s: &str) -> UserId {
    UserId::parse(s).expect("user id")
}

fn d(who: &str) -> ConvKey {
    ConvKey::Direct(uid(who))
}

fn harness(dir: &tempfile::TempDir) -> IngestHarness {
    IngestHarness::open(
        &dir.path().join("core.db"),
        &kourier_db::DbKey::from([5; 32]),
        NetworkKind::Fake,
        MsgIdScope::AccountAuthor,
        &[uid(ME)],
    )
    .expect("harness")
}

/// History rows of `who`'s chat, one per text, `local` = `<prefix><n>`.
fn history(who: &str, prefix: &str, texts: &[&str]) -> BackendEvent {
    let messages = texts
        .iter()
        .enumerate()
        .map(|(n, text)| {
            let n = i64::try_from(n).expect("n");
            let mut m = remote(
                &d(who),
                &uid(who),
                &format!("{prefix}{n}"),
                T0 + n * 1000,
                text,
            );
            m.read = true;
            m
        })
        .collect();
    BackendEvent::HistoryBatch {
        conv: d(who),
        messages,
        has_more: false,
        progress: None,
    }
}

fn msg(h: &IngestHarness, local: &str) -> MessageId {
    h.connection()
        .query_row(
            "SELECT id FROM messages WHERE net_local = ?1",
            [local],
            |r| r.get(0).map(MessageId),
        )
        .expect("message")
}

fn chat(h: &IngestHarness, who: &str) -> ConversationId {
    h.connection()
        .query_row(
            "SELECT id FROM conversations WHERE conv_key = ?1",
            [d(who).encode()],
            |r| r.get(0).map(ConversationId),
        )
        .expect("chat")
}

fn flush(h: &mut IngestHarness) {
    h.search_flush().expect("flush");
    assert_eq!(search::queued(h.connection()).expect("queue"), 0);
}

fn ask(text: &str, chats: Option<Vec<ConversationId>>) -> SearchQuery {
    SearchQuery {
        text: Redacted::new(text.to_owned()),
        chats,
        limit: MAX_SEARCH_PAGE,
        before: None,
        mode: Default::default(),
    }
}

/// One page and how many rows of the index it looked up.
fn page(h: &IngestHarness, q: &SearchQuery) -> (SearchPage, usize) {
    search::page(h.search_db(), h.connection(), q, search::CAP).expect("search")
}

fn ids(p: &SearchPage) -> Vec<MessageId> {
    let mut v: Vec<MessageId> = p.hits.iter().map(|h| h.message).collect();
    v.sort_unstable_by_key(|m| m.0);
    v
}

fn found(h: &IngestHarness, text: &str) -> Vec<MessageId> {
    ids(&page(h, &ask(text, None)).0)
}

#[test]
fn hostile_text_reaches_the_match_as_plain_words_and_widens_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&dir);
    h.apply(vec![history(
        ALICE,
        "a",
        &[
            "alphaq betaq",
            "alphaq",
            "betaq",
            "near alphaq betaq",
            "alphaq or betaq",
            "secretq",
        ],
    )])
    .expect("apply");
    flush(&mut h);
    let m = |n: usize| msg(&h, &format!("a{n}"));
    let alpha = vec![m(0), m(1), m(3), m(4)];
    let long_words = "alphaq ".repeat(15_000);
    let long_junk = "\"(".repeat(50_000);
    let cases: Vec<(&str, Vec<MessageId>)> = vec![
        ("\"", vec![]),
        ("*", vec![]),
        (":", vec![]),
        ("(", vec![]),
        ("{", vec![]),
        ("\u{0}", vec![]),
        ("!@#$%^&*()_+-=[]{};':\",./<>?\\|~`", vec![]),
        // Not the operator: "near" is a word that must be in the row too.
        ("NEAR(alphaq betaq)", vec![m(3)]),
        ("alphaq OR betaq", vec![m(4)]),
        ("alphaq AND betaq", vec![]),
        ("alphaq NOT betaq", vec![]),
        ("-betaq alphaq", vec![m(0), m(3), m(4)]),
        // No column filter of his: `words` and `meta` are plain words, the chat's token is
        // never reached.
        ("words:secretq", vec![]),
        ("meta:c1", vec![]),
        ("c1", vec![]),
        ("\"alphaq", alpha.clone()),
        ("alphaq*", alpha.clone()),
        ("alphaq\u{0}", alpha.clone()),
        ("{alphaq}", alpha.clone()),
        (long_words.as_str(), alpha.clone()),
        (long_junk.as_str(), vec![]),
    ];
    for (text, want) in cases {
        let (p, _) = page(&h, &ask(text, None));
        let shown = if text.len() > 60 { "(long)" } else { text };
        assert_eq!(ids(&p), want, "{shown:?}");
        assert!(!p.capped && p.next.is_none(), "{shown:?}");
    }
}

/// Each rule of the query hides an INDEXED row (the raw index still holds it), so the row is
/// left out by the query, not by the index.
#[test]
fn the_query_time_rules_leave_out_what_can_flip() {
    let flips: [(&str, &str); 7] = [
        ("deleted for me", "UPDATE messages SET deleted = 'ForMe'"),
        (
            "deleted for everyone",
            "UPDATE messages SET deleted = 'ForEveryone'",
        ),
        ("view-once", "UPDATE messages SET view_once = 1"),
        (
            "hidden on an unconfirmed clock",
            "UPDATE messages SET expiry_hidden = 1",
        ),
        ("a chat deleted here", "UPDATE conversations SET hidden = 1"),
        ("a protected chat", "UPDATE conversations SET protected = 1"),
        (
            "an account being removed",
            "UPDATE accounts SET state = 'Removing'",
        ),
    ];
    for (what, flip) in flips {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut h = harness(&dir);
        h.apply(vec![history(ALICE, "r", &["ruleq here"])])
            .expect("apply");
        flush(&mut h);
        assert_eq!(found(&h, "ruleq").len(), 1, "{what}: found first");
        h.connection().execute(flip, []).expect("flip");
        // No flush: the index keeps the row; only the query leaves it out.
        assert_eq!(
            search::ids_with(h.search_db(), "ruleq").expect("raw").len(),
            1
        );
        assert!(
            found(&h, "ruleq").is_empty(),
            "{what}: left out by the query"
        );
        // And under a scope too.
        let conv = chat(&h, ALICE);
        assert!(
            page(&h, &ask("ruleq", Some(vec![conv]))).0.hits.is_empty(),
            "{what}: left out in one chat"
        );
    }
    // An `Unlinking` account's chats are listed, so they are searched (L11).
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&dir);
    h.apply(vec![history(ALICE, "r", &["ruleq here"])])
        .expect("apply");
    flush(&mut h);
    h.connection()
        .execute("UPDATE accounts SET state = 'Unlinking'", [])
        .expect("flip");
    assert_eq!(found(&h, "ruleq").len(), 1);
}

#[test]
fn a_scope_looks_up_no_row_of_another_chat_and_its_answer_does_not_move() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&dir);
    let others: Vec<String> = (0..50).map(|n| format!("scopeq other{n}")).collect();
    let others: Vec<&str> = others.iter().map(String::as_str).collect();
    h.apply(vec![
        history(ALICE, "a", &["scopeq one", "scopeq two", "scopeq three"]),
        history(CAROL, "c", &others),
    ])
    .expect("apply");
    flush(&mut h);
    let a = chat(&h, ALICE);
    let (first, looked) = page(&h, &ask("scopeq", Some(vec![a])));
    assert_eq!(first.hits.len(), 3);
    assert!(first.hits.iter().all(|hit| hit.conv == a));
    assert_eq!(looked, 3, "no row of the other chat is looked up");
    // The other chat's text changes; the answer in the scope is the same, byte for byte.
    h.connection()
        .execute(
            "UPDATE messages SET body = ?1 WHERE conversation_id <> ?2",
            (
                serde_json::to_string(&Body::Text(Text::plain("scopeq changed words")))
                    .expect("json"),
                a.0,
            ),
        )
        .expect("rewrite");
    flush(&mut h);
    let (again, looked) = page(&h, &ask("scopeq", Some(vec![a])));
    assert_eq!(again, first);
    assert_eq!(looked, 3);
    // Every chat sees them all; an empty list of chats sees none (fail closed, M11).
    assert_eq!(page(&h, &ask("scopeq", None)).0.hits.len(), 53);
    let (none, looked) = page(&h, &ask("scopeq", Some(vec![])));
    assert!(none.hits.is_empty() && none.next.is_none() && !none.capped);
    assert_eq!(looked, 0);
}

/// A merge moves rows before the flush changes their chat's token (M11): a scope of the old
/// chat must not show them.
#[test]
fn a_row_that_moved_to_another_chat_is_not_shown_under_the_old_one() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&dir);
    h.apply(vec![
        history(ALICE, "a", &["movedq here"]),
        history(CAROL, "c", &["something else"]),
    ])
    .expect("apply");
    flush(&mut h);
    let (a, c) = (chat(&h, ALICE), chat(&h, CAROL));
    assert_eq!(page(&h, &ask("movedq", Some(vec![a]))).0.hits.len(), 1);
    h.connection()
        .execute(
            "UPDATE messages SET conversation_id = ?1 WHERE net_local = 'a0'",
            [c.0],
        )
        .expect("move");
    assert!(page(&h, &ask("movedq", Some(vec![a]))).0.hits.is_empty());
    let all = page(&h, &ask("movedq", None)).0;
    assert_eq!(all.hits.len(), 1);
    assert_eq!(
        all.hits[0].conv, c,
        "a hit names the chat the row is in now"
    );
}

/// MIN_PREFIX = 2 (his answer "From two letters"): one letter finds the word itself only, and
/// the MATCH does not even step the rows a prefix would find.
#[test]
fn one_letter_finds_its_word_and_two_letters_find_the_words_they_start() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&dir);
    h.apply(vec![history(
        ALICE,
        "s",
        &["a word", "apple", "ab abc", "ap"],
    )])
    .expect("apply");
    flush(&mut h);
    let m = |n: usize| msg(&h, &format!("s{n}"));
    let (p, looked) = page(&h, &ask("a", None));
    assert_eq!(ids(&p), vec![m(0)]);
    assert_eq!(looked, 1, "a one-letter term is exact in the MATCH");
    assert_eq!(found(&h, "ap"), vec![m(1), m(3)]);
    assert_eq!(found(&h, "ab"), vec![m(2)]);
    assert_eq!(found(&h, "abc"), vec![m(2)]);
    assert_eq!(found(&h, "AppL"), vec![m(1)]);
}

#[test]
fn a_stale_row_and_scattered_ideographs_are_dropped() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&dir);
    h.apply(vec![history(
        ALICE,
        "j",
        &["日本語のテキスト", "日 x 本 y 語", "staleq word"],
    )])
    .expect("apply");
    flush(&mut h);
    let m = |n: usize| msg(&h, &format!("j{n}"));
    // The index finds both; the snippet keeps only the row where the three stand side by side.
    let (p, looked) = page(&h, &ask("日本語", None));
    assert_eq!(ids(&p), vec![m(0)]);
    assert_eq!(looked, 2);
    assert_eq!(found(&h, "日 本"), vec![m(0), m(1)]);
    // A body rewritten under the index (no flush yet): the old word finds nothing.
    h.connection()
        .execute(
            "UPDATE messages SET body = ?1 WHERE net_local = 'j2'",
            [serde_json::to_string(&Body::Text(Text::plain("freshq word"))).expect("json")],
        )
        .expect("rewrite");
    let (p, looked) = page(&h, &ask("staleq", None));
    assert!(p.hits.is_empty());
    assert_eq!(looked, 1, "the row was looked up and dropped as stale");
}

#[test]
fn a_hit_carries_its_chat_sender_time_and_snippet() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&dir);
    h.apply(vec![history(ALICE, "x", &["the hitwordq is here"])])
        .expect("apply");
    flush(&mut h);
    let (p, _) = page(&h, &ask("hitw", None));
    assert_eq!(p.hits.len(), 1);
    let hit = &p.hits[0];
    assert_eq!(hit.message, msg(&h, "x0"));
    assert_eq!(hit.conv, chat(&h, ALICE));
    assert_eq!(hit.account, h.account());
    assert_eq!(hit.sent_at, T0);
    assert!(!hit.own);
    assert_eq!(hit.author, Some(uid(ALICE)));
    assert_eq!(hit.snippet.text, "the hitwordq is here");
    let mark = hit.snippet.marks[0];
    let (s, e) = (mark.start as usize, (mark.start + mark.len) as usize);
    assert_eq!(&hit.snippet.text[s..e], "hitwordq");
    assert!(p.index.ready && p.index.percent == 100);
    // Nothing of the text in a Debug (§9.1).
    let shown = format!("{:?} {:?}", ask("hitwordq", None), p);
    assert!(
        !shown.contains("hitwordq") && !shown.contains("here"),
        "{shown}"
    );
}

/// The rows of `rows` (hour, tie) in two chats, every one holding `commonq`; returns the ids
/// in the brute-force order of the search (`sort_key` then id, descending).
fn world(h: &mut IngestHarness, rows: &[(i64, i64)]) -> Vec<MessageId> {
    let texts: Vec<String> = (0..rows.len()).map(|n| format!("commonq row{n}")).collect();
    let (a, c): (Vec<_>, Vec<_>) = texts.iter().enumerate().partition(|(n, _)| n % 3 != 0);
    let a: Vec<&str> = a.into_iter().map(|(_, t)| t.as_str()).collect();
    let c: Vec<&str> = c.into_iter().map(|(_, t)| t.as_str()).collect();
    h.apply(vec![history(ALICE, "a", &a), history(CAROL, "c", &c)])
        .expect("apply");
    // Sort keys chosen by the case: several hours, ties inside an hour. (Before the flush: the
    // queue's entry names a new row, the flush takes the key the row has then.)
    let all: Vec<i64> = h
        .connection()
        .prepare("SELECT id FROM messages ORDER BY id")
        .expect("prepare")
        .query_map([], |r| r.get(0))
        .expect("ids")
        .collect::<Result<_, _>>()
        .expect("ids");
    assert_eq!(all.len(), rows.len());
    let mut keyed: Vec<(i64, i64)> = Vec::new();
    for (id, (hour, tie)) in all.iter().zip(rows) {
        let key = (T0 * 1024 / HOUR + hour) * HOUR + tie * 1024;
        h.connection()
            .execute("UPDATE messages SET sort_key = ?1 WHERE id = ?2", (key, id))
            .expect("key");
        keyed.push((key, *id));
    }
    flush(h);
    keyed.sort_unstable_by(|x, y| y.cmp(x));
    keyed.into_iter().map(|(_, id)| MessageId(id)).collect()
}

/// Every page of `text` at `limit`, with at most `cap` rows looked up per page, until `next`
/// is `None`; fails on a walk that does not end.
fn walk(h: &IngestHarness, text: &str, limit: u32, cap: usize) -> (Vec<MessageId>, usize) {
    let mut q = ask(text, None);
    q.limit = limit;
    let (mut out, mut capped) = (Vec::new(), 0);
    for _ in 0..10_000 {
        let (p, _) = search::page(h.search_db(), h.connection(), &q, cap).expect("page");
        assert!(p.hits.len() <= limit as usize);
        capped += usize::from(p.capped);
        out.extend(p.hits.iter().map(|hit| hit.message));
        match p.next {
            Some(next) => q.before = Some(next),
            None => return (out, capped),
        }
    }
    panic!("the walk did not end");
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]

    /// The pages of a search, one after another, are the brute-force sort; with the page's
    /// lookups capped at 20 (H4) every hit still comes exactly once and the walk ends.
    #[test]
    fn pages_are_the_brute_force_sort_and_a_capped_walk_gives_every_hit_once(
        rows in prop::collection::vec((0_i64..4, 0_i64..3), 1..120),
        limit in 1_u32..9,
    ) {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut h = harness(&dir);
        let want = world(&mut h, &rows);
        let (got, capped) = walk(&h, "commonq", limit, search::CAP);
        prop_assert_eq!(&got, &want);
        prop_assert_eq!(capped, 0);
        let (got, _) = walk(&h, "comm", limit, 20);
        let mut sorted = got.clone();
        sorted.sort_unstable_by_key(|m| m.0);
        sorted.dedup();
        prop_assert_eq!(sorted.len(), got.len(), "no hit twice");
        let mut all = want.clone();
        all.sort_unstable_by_key(|m| m.0);
        prop_assert_eq!(sorted, all, "every hit");
    }
}

/// One hour holds more rows than a page may look up (H4): the walk goes on inside it, in id
/// order, and every hit comes once.
#[test]
fn an_hour_over_the_cap_is_walked_to_its_end() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&dir);
    let mut rows: Vec<(i64, i64)> = (0..60).map(|n| (1, n % 7)).collect();
    rows.extend((0..5).map(|n| (0, n)));
    rows.extend((0..5).map(|n| (2, n)));
    let want = world(&mut h, &rows);
    let (got, capped) = walk(&h, "commonq", 7, 20);
    assert!(capped >= 1, "the busy hour capped a page: {capped}");
    let mut sorted = got.clone();
    sorted.sort_unstable_by_key(|m| m.0);
    sorted.dedup();
    assert_eq!(sorted.len(), got.len(), "no hit twice");
    let mut all = want.clone();
    all.sort_unstable_by_key(|m| m.0);
    assert_eq!(sorted, all);
    // The newest hour first (whole and in time order), the oldest last.
    assert_eq!(got[..5], want[..5]);
    assert_eq!(got[got.len() - 5..], want[want.len() - 5..]);
}

#[test]
fn a_cursor_holds_across_rows_that_come_between_two_pages() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&dir);
    let rows: Vec<(i64, i64)> = (0..12).map(|n| (n % 3, n)).collect();
    let want = world(&mut h, &rows);
    let mut q = ask("commonq", None);
    q.limit = 5;
    let (first, _) = search::page(h.search_db(), h.connection(), &q, search::CAP).expect("page");
    let shown: Vec<MessageId> = first.hits.iter().map(|hit| hit.message).collect();
    assert_eq!(shown, want[..5]);
    // A newer row and an older one arrive between the pages.
    h.apply(vec![
        history(ALICE, "new", &["commonq newer"]),
        history(CAROL, "old", &["commonq older"]),
    ])
    .expect("apply");
    let newer_key = (T0 * 1024 / HOUR + 10) * HOUR;
    for (local, key) in [("new0", newer_key), ("old0", 1024)] {
        h.connection()
            .execute(
                "UPDATE messages SET sort_key = ?1 WHERE net_local = ?2",
                (key, local),
            )
            .expect("key");
    }
    flush(&mut h);
    let older = msg(&h, "old0");
    q.before = first.next;
    let (second, _) = search::page(h.search_db(), h.connection(), &q, search::CAP).expect("page");
    let mut rest = Vec::new();
    loop {
        let (p, _) = search::page(h.search_db(), h.connection(), &q, search::CAP).expect("page");
        rest.extend(p.hits.iter().map(|hit| hit.message));
        match p.next {
            Some(next) => q.before = Some(next),
            None => break,
        }
    }
    let mut expected = want[5..].to_vec();
    expected.push(older);
    assert_eq!(
        rest, expected,
        "the rest in order, the older row last, the newer one not"
    );
    // A cursor is plain data: the same page again from the same cursor.
    q.before = first.next;
    let (again, _) = search::page(h.search_db(), h.connection(), &q, search::CAP).expect("page");
    assert_eq!(again, second);
}

/// The plans: FTS5 serves the order and the bound (no sort), and each candidate is read by
/// primary key in `messages`, `conversations` and `accounts`.
#[test]
fn the_search_reads_by_primary_key_and_sorts_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let h = harness(&dir);
    for (name, plan) in search::plans(h.search_db(), h.connection()).expect("plans") {
        assert!(!plan.contains("TEMP B-TREE"), "{name}: {plan}");
        match name {
            "rowids" => assert!(plan.contains("VIRTUAL TABLE"), "{plan}"),
            // The pattern scan of one chat walks the timeline index; its chat and account by key.
            "timeline_scan" => {
                assert!(
                    plan.contains("SEARCH m USING INDEX messages_timeline (conversation_id=?"),
                    "{plan}"
                );
                for t in ["c", "a"] {
                    assert!(
                        plan.contains(&format!("SEARCH {t} USING INTEGER PRIMARY KEY")),
                        "{t}: {plan}"
                    );
                }
                assert!(!plan.contains("SCAN"), "{plan}");
            }
            // The hit read and the pattern scan of every chat: all three tables by key.
            _ => {
                for t in ["m", "c", "a"] {
                    assert!(
                        plan.contains(&format!("SEARCH {t} USING INTEGER PRIMARY KEY")),
                        "{t}: {plan}"
                    );
                }
                assert!(!plan.contains("SCAN"), "{plan}");
            }
        }
    }
}

/// Through the running core: the search's own read connection (SQLCipher, read-only) answers
/// with the index's state, and opens again after the memory was given back.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_running_core_answers_a_search_on_its_own_connection() {
    let me = uid(ME);
    let script = Script::new("search", vec![me]).deliver(vec![BackendEvent::Message(remote(
        &d(ALICE),
        &uid(ALICE),
        "1",
        T0 + 1000,
        "a corequeryq word",
    ))]);
    let mut env = Env::new(Flavor::SignalLike, script).await;
    let account = add_linked(&mut env).await;
    env.wait_finished(account).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    // The row came before search.db (made at UiReady): the backfill writes it after its delay.
    eventually("the row is indexed", WAIT * 2, || {
        // search.db may exist a moment before its schema is committed.
        let n = env.search_db().exists()
            && env
                .try_peek_search(|c| search::ids_with(c, "corequeryq"))
                .is_some_and(|ids| ids.len() == 1);
        async move { n }
    })
    .await;
    let asked = || Query::Search(ask("corequ", None));
    let hits = |data: QueryData| match data {
        QueryData::Search(p) => p,
        other => panic!("unexpected {other:?}"),
    };
    let p = hits(env.handle.query(asked()).await.expect("search"));
    assert_eq!(p.hits.len(), 1);
    assert_eq!(p.hits[0].account, account);
    assert!(p.index.ready);
    env.handle.send(UiCommand::ReleaseMemory).expect("release");
    let again = hits(env.handle.query(asked()).await.expect("search"));
    assert_eq!(again, p);
    let none = hits(
        env.handle
            .query(Query::Search(ask("corequeryq", Some(vec![]))))
            .await
            .expect("search"),
    );
    assert!(none.hits.is_empty());
    env.stop().await;
}
