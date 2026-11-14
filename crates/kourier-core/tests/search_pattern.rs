//! Pattern search (R5, architecture §8.4): `SearchMode::Pattern` scans `messages` under the
//! word search's query-time rules and runs the filters' regex over what search reads. A pattern
//! finds what words cannot; protected, deleted, view-once and hidden rows never; a scope; a
//! refused pattern is typed and never quoted; `capped`/`next` over a big fixture; pages by
//! `limit` give every hit once, newest first.

use std::time::{Duration, Instant};

use kourier_backend_api::BackendEvent;
use kourier_core::error::CoreError;
use kourier_core::facade::filters::{Field, PatternErrorKind};
use kourier_core::facade::{FilterRefusal, MAX_SEARCH_PAGE, SearchMode, SearchPage, SearchQuery};
use kourier_core::testing::{IngestHarness, search};
use kourier_fake::scripts::{T0, remote};
use kourier_model::{
    ConvKey, ConversationId, MessageId, MsgIdScope, NetworkKind, Redacted, UserId,
};

const ME: &str = "aci:00000000-0000-4000-8000-0000000000aa";
const ALICE: &str = "aci:00000000-0000-4000-8000-0000000a11ce";
const CAROL: &str = "aci:00000000-0000-4000-8000-0000000ca202";

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

fn chat(h: &IngestHarness, who: &str) -> ConversationId {
    h.connection()
        .query_row(
            "SELECT id FROM conversations WHERE conv_key = ?1",
            [d(who).encode()],
            |r| r.get(0).map(ConversationId),
        )
        .expect("chat")
}

fn ask(pattern: &str, match_case: bool, chats: Option<Vec<ConversationId>>) -> SearchQuery {
    SearchQuery {
        text: Redacted::new(pattern.to_owned()),
        chats,
        limit: MAX_SEARCH_PAGE,
        before: None,
        mode: SearchMode::Pattern { match_case },
    }
}

fn far() -> Instant {
    Instant::now() + Duration::from_secs(3600)
}

fn run(h: &IngestHarness, q: &SearchQuery) -> SearchPage {
    search::page_by(h.search_db(), h.connection(), q, far()).expect("search")
}

fn ids(p: &SearchPage) -> Vec<MessageId> {
    p.hits.iter().map(|h| h.message).collect()
}

fn sorted(mut v: Vec<MessageId>) -> Vec<MessageId> {
    v.sort_unstable_by_key(|m| m.0);
    v
}

#[test]
fn a_pattern_finds_what_words_cannot_and_marks_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&dir);
    h.apply(vec![history(
        ALICE,
        "a",
        &[
            "a COLOUR swatch",
            "one color only",
            "nothing here",
            "pay 123-45 now",
        ],
    )])
    .expect("apply");
    h.search_flush().expect("flush");
    let p = run(&h, &ask(r"colou?r", false, None));
    assert_eq!(p.hits.len(), 2, "both spellings");
    assert!(
        p.pattern.is_some(),
        "the page hands the compiled pattern back"
    );
    assert!(p.next.is_none());
    for hit in &p.hits {
        let m = hit.snippet.marks.first().expect("a mark");
        let word = &hit.snippet.text[m.start as usize..(m.start + m.len) as usize];
        assert!(word.eq_ignore_ascii_case("colour") || word.eq_ignore_ascii_case("color"));
    }
    // Case: ignored by default, kept by the tick.
    assert_eq!(run(&h, &ask("COLOUR", false, None)).hits.len(), 1);
    assert_eq!(run(&h, &ask("colour", true, None)).hits.len(), 0);
    assert_eq!(run(&h, &ask("COLOUR", true, None)).hits.len(), 1);
    // A shape no word search can say.
    assert_eq!(run(&h, &ask(r"\d{3}-\d{2}", false, None)).hits.len(), 1);
    // An empty pattern is no search.
    let p = run(&h, &ask("", false, None));
    assert!(p.hits.is_empty() && p.next.is_none());
}

#[test]
fn protected_deleted_view_once_and_hidden_rows_are_never_found() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&dir);
    h.apply(vec![
        history(
            ALICE,
            "a",
            &["zebra one", "zebra two", "zebra three", "zebra four"],
        ),
        history(CAROL, "c", &["zebra carol"]),
    ])
    .expect("apply");
    h.search_flush().expect("flush");
    let conn = h.connection();
    assert_eq!(run(&h, &ask("zebra", false, None)).hits.len(), 5);
    conn.execute(
        "UPDATE messages SET deleted = 'ForMe' WHERE net_local = 'a0'",
        [],
    )
    .expect("deleted");
    conn.execute(
        "UPDATE messages SET view_once = 1 WHERE net_local = 'a1'",
        [],
    )
    .expect("view once");
    conn.execute(
        "UPDATE messages SET expiry_hidden = 1 WHERE net_local = 'a2'",
        [],
    )
    .expect("hidden");
    let left = run(&h, &ask("zebra", false, None));
    assert_eq!(left.hits.len(), 2, "a3 and carol's");
    conn.execute(
        "UPDATE conversations SET protected = 1 WHERE conv_key = ?1",
        [d(CAROL).encode()],
    )
    .expect("protected");
    assert_eq!(run(&h, &ask("zebra", false, None)).hits.len(), 1);
    // A search limited to a chat is limited to it, and the protected chat stays out of the
    // timeline walk too (the rule is the shared SQL).
    let alice = chat(&h, ALICE);
    assert_eq!(
        run(&h, &ask("zebra", false, Some(vec![alice]))).hits.len(),
        1
    );
    let carol = chat(&h, CAROL);
    assert!(
        run(&h, &ask("zebra", false, Some(vec![carol])))
            .hits
            .is_empty()
    );
    // A grant of no chat is no grant.
    let none = run(&h, &ask("zebra", false, Some(Vec::new())));
    assert!(none.hits.is_empty() && none.next.is_none());
}

#[test]
fn a_refused_pattern_is_typed_and_never_quoted() {
    let dir = tempfile::tempdir().expect("tempdir");
    let h = harness(&dir);
    let marker = "qzmarkerq";
    for (pattern, kind) in [
        (format!("({marker}"), PatternErrorKind::UnclosedGroup),
        (
            format!("(?={marker})"),
            PatternErrorKind::LookAroundUnsupported,
        ),
        (format!("*{marker}"), PatternErrorKind::NothingToRepeat),
    ] {
        let q = ask(&pattern, false, None);
        let err = search::page_by(h.search_db(), h.connection(), &q, far())
            .expect_err("the pattern is refused");
        match &err {
            CoreError::Filter(FilterRefusal::Pattern(Field::Text, e)) => {
                assert_eq!(e.kind, kind)
            }
            other => panic!("not typed: {other:?}"),
        }
        let shown = format!("{err} {err:?}");
        assert!(
            !shown.contains(marker),
            "the pattern is never quoted: {shown}"
        );
    }
    let long = "a".repeat(kourier_core::facade::filters::MAX_PATTERN_CHARS + 1);
    let err = search::page_by(
        h.search_db(),
        h.connection(),
        &ask(&long, false, None),
        far(),
    )
    .expect_err("too long");
    assert!(matches!(
        err,
        CoreError::Filter(FilterRefusal::PatternTooLong(Field::Text))
    ));
}

/// A big fixture, the target at the very bottom: a page whose time is up is `capped` with a
/// `next`, and following the cursors finds the target once and ends.
#[test]
fn a_page_that_runs_out_of_time_goes_on_from_where_it_stopped() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&dir);
    let mut texts: Vec<String> = vec!["needle7 at the bottom".to_owned()];
    texts.extend((0..1_500).map(|n| format!("filler number {n}")));
    let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
    h.apply(vec![history(ALICE, "a", &refs)]).expect("apply");
    h.search_flush().expect("flush");

    let mut q = ask("needle7", false, None);
    let past = Instant::now() - Duration::from_secs(1);
    let mut pages = 0;
    let mut hits = Vec::new();
    loop {
        let p = search::page_by(h.search_db(), h.connection(), &q, past).expect("page");
        pages += 1;
        assert!(pages < 100, "the walk ends");
        hits.extend(ids(&p));
        if pages == 1 {
            assert!(p.capped, "out of time: capped");
        }
        match p.next {
            Some(next) => q.before = Some(next),
            None => break,
        }
    }
    assert_eq!(hits.len(), 1, "the target comes once");
    assert!(pages > 3, "the clock stopped the pages ({pages})");
}

#[test]
fn pages_by_limit_give_every_hit_once_newest_first() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&dir);
    let texts: Vec<String> = (0..7).map(|n| format!("lemur {n}")).collect();
    let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
    h.apply(vec![
        history(ALICE, "a", &refs),
        history(CAROL, "c", &["lemur c0", "lemur c1"]),
    ])
    .expect("apply");
    h.search_flush().expect("flush");
    for scope in [None, Some(vec![chat(&h, ALICE)])] {
        let mut q = ask("lemur", false, scope.clone());
        q.limit = 2;
        let mut all = Vec::new();
        for _ in 0..20 {
            let p = run(&h, &q);
            assert!(p.hits.len() <= 2);
            all.extend(p.hits.iter().map(|h| (h.sort_key, h.message)));
            match p.next {
                Some(n) => q.before = Some(n),
                None => break,
            }
        }
        let want = if scope.is_none() { 9 } else { 7 };
        assert_eq!(all.len(), want, "every hit");
        let mut dedup: Vec<i64> = all.iter().map(|(_, m)| m.0).collect();
        dedup.sort_unstable();
        dedup.dedup();
        assert_eq!(dedup.len(), want, "none twice");
        if scope.is_some() {
            assert!(
                all.windows(2).all(|w| w[0] > w[1]),
                "newest first by the timeline"
            );
        }
        assert_eq!(sorted(all.iter().map(|(_, m)| *m).collect()).len(), want);
    }
}

/// R6: what the pattern scan costs at 100,000 messages (run by name, in release, under kcap):
/// messages scanned per second for a pattern that matches nothing, driven to the end of the
/// store, and the first page's time for a pattern that matches 1 message in 100. Synthetic
/// texts, written straight into `messages` (the cheapest way: no index is needed, the scan
/// reads `messages`). Prints counts and timings only.
#[test]
#[ignore = "a measurement (R6): run by name, in release, under kcap"]
fn pattern_scan_timing_at_100k_messages() {
    use kourier_core::testing::filter_mix_word;
    const ROWS: i64 = 100_000;
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&dir);
    let mut chats = Vec::new();
    for i in 0..20 {
        let who = format!("aci:00000000-0000-4000-8000-{:012x}", 0x2000 + i);
        h.apply(vec![history(&who, "o", &["opener"])])
            .expect("chat");
        chats.push((
            who,
            chat(
                &h,
                &format!("aci:00000000-0000-4000-8000-{:012x}", 0x2000 + i),
            ),
        ));
    }
    let account = h.account().0;
    let t = Instant::now();
    {
        let conn = h.connection();
        let tx = conn.unchecked_transaction().expect("tx");
        {
            let mut insert = tx
                .prepare(
                    "INSERT INTO messages(account_id, conversation_id, net_scope, net_author, net_local,
                         origin, sort_key, sent_at, received_at, body_kind, body)
                       VALUES (?1, ?2, 0, ?3, ?4, 'Remote', ?5 * 1024, ?5, ?5, 'Text', ?6)",
                )
                .expect("prepare");
            for i in 0..ROWS {
                let (who, id) = &chats[(i % 20) as usize];
                let mut text = String::new();
                for k in 0..(8 + i % 17) {
                    text.push_str(&filter_mix_word(
                        (i * 13 + k) as usize % 40_000 + 500,
                        i % 4 == 0,
                    ));
                    text.push(' ');
                }
                if i % 100 == 5 {
                    text.push_str("zzneedle");
                }
                let body = format!("{{\"Text\":{{\"body\":\"{text}\"}}}}");
                insert
                    .execute((
                        account,
                        id.0,
                        who,
                        format!("p{i}"),
                        T0 + 100_000 + i * 1000,
                        body,
                    ))
                    .expect("insert");
            }
        }
        tx.commit().expect("commit");
    }
    println!(
        "pattern_scan_timing: {ROWS} messages seeded in {:.1} s",
        t.elapsed().as_secs_f64()
    );

    // A pattern that matches nothing, to the end.
    let mut q = ask(r"\bqqqq\d{4}\b", false, None);
    let t = Instant::now();
    let (mut pages, mut hits) = (0, 0);
    loop {
        let p = run(&h, &q);
        pages += 1;
        hits += p.hits.len();
        match p.next {
            Some(n) => q.before = Some(n),
            None => break,
        }
        assert!(pages < 100_000, "the walk ends");
    }
    let secs = t.elapsed().as_secs_f64();
    println!(
        "pattern_scan_timing: no-match scan: {pages} pages, {hits} hits, {secs:.2} s = {:.0} messages per second",
        ROWS as f64 / secs
    );

    // The first page of a pattern that matches 1 in 100.
    let q = ask("zzneedle", false, None);
    for round in 0..3 {
        let t = Instant::now();
        let p = run(&h, &q);
        println!(
            "pattern_scan_timing: first page of a 1-in-100 pattern, round {round}: {:.1} ms, {} hits, capped {}",
            t.elapsed().as_secs_f64() * 1000.0,
            p.hits.len(),
            p.capped
        );
    }
}
