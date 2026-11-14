//! RegExp filters, R1: the table (`_filters`, found by its NAME), the commands, the query, the
//! event, the validation's refusals, the order, the compiled set's answers through the core,
//! a restart, and the log (a unique marker pattern and name never reach it).

use std::sync::{Arc, Mutex};

use kourier_core::facade::{
    ActionSubject, Filter, FilterId, FilterList, FilterRefusal, FilterSpec, FilterTexts, Query,
    QueryData, UiCommand, UiEvent,
    filters::{Field, MAX_NAME_CHARS, MAX_PATTERN_CHARS, PatternErrorKind},
};
use kourier_fake::{Flavor, scripts};
use kourier_model::Redacted;
use tracing_subscriber::Layer;
use tracing_subscriber::layer::SubscriberExt;

use crate::support::{self, Env};

const STEP: &str = "_filters";

fn spec(name: &str, field: Field, pattern: &str) -> FilterSpec {
    let p = Some(Redacted::new(pattern.to_owned()));
    let mut s = FilterSpec {
        name: Redacted::new(name.to_owned()),
        colour: 0xFFD400,
        highlight: true,
        ..Default::default()
    };
    match field {
        Field::Text => s.text_pattern = p,
        Field::Sender => s.sender_pattern = p,
        Field::Chat => s.chat_pattern = p,
    }
    s
}

async fn env() -> Env {
    Env::new(Flavor::SignalLike, scripts::signal_like().expect("script")).await
}

async fn list(env: &Env) -> FilterList {
    match env.handle.query(Query::Filters).await.expect("query") {
        QueryData::Filters(l) => l,
        other => panic!("unexpected {other:?}"),
    }
}

fn names(l: &FilterList) -> Vec<String> {
    l.filters
        .iter()
        .map(|f| f.spec.name.expose().clone())
        .collect()
}

fn positions(env: &Env) -> Vec<(i64, String)> {
    env.peek(|c| {
        c.prepare("SELECT position, name FROM filters ORDER BY position")
            .expect("prepare")
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .expect("query")
            .collect::<Result<_, _>>()
            .expect("rows")
    })
}

/// Sends a save and waits for its outcome: the new revision, or the refusal.
async fn save(env: &mut Env, id: Option<FilterId>, spec: FilterSpec) -> Result<u64, FilterRefusal> {
    command(env, UiCommand::SaveFilter { id, spec }, "SaveFilter").await
}

/// Sends a filter command and waits for ITS outcome: a revision above the one the core held
/// before it (read with [`list`], which waits for the start's set), or the refusal. The start's
/// own compile announces revision 1 by itself (R6b), after the set is ready, so that event can
/// still be on its way when the first command goes: taken for the command's, it failed the full
/// ci of 2026-11-14 ("every change moves the revision", r1 == r0).
async fn command(
    env: &mut Env,
    cmd: UiCommand,
    action: &'static str,
) -> Result<u64, FilterRefusal> {
    let floor = list(env).await.revision;
    env.handle.send(cmd).expect("send");
    outcome(env, action, floor).await
}

/// The outcome of `action`: a `FiltersChanged` above `floor`, or its refusal.
async fn outcome(env: &mut Env, action: &'static str, floor: u64) -> Result<u64, FilterRefusal> {
    env.event("the outcome", |e| match e {
        UiEvent::FiltersChanged { revision } if *revision > floor => Some(Ok(*revision)),
        UiEvent::ActionFailed {
            action: a,
            error,
            subject: ActionSubject::None,
            account: None,
        } if *a == action => Some(Err(error.filter_refusal().expect("a filter refusal"))),
        _ => None,
    })
    .await
}

fn texts<'a>(text: &'a str) -> FilterTexts<'a> {
    FilterTexts {
        text: Some(text),
        sender: Some("Alice"),
        chat: Some("Family"),
    }
}

fn hits(l: &FilterList, t: FilterTexts<'_>) -> Vec<usize> {
    l.set.matches(&t).collect()
}

/// The step is by name; the table is there at the head with its columns, positions are unique,
/// a migrated older file gets it empty.
#[test]
fn the_migration_adds_an_empty_table_with_a_unique_position() {
    use kourier_db::rusqlite::{Connection, ErrorCode};
    let steps = &kourier_core::MIGRATIONS.steps;
    let at = steps
        .iter()
        .position(|m| m.name.ends_with(STEP))
        .expect("the step is registered");
    let dir = tempfile::tempdir().expect("tempdir");
    let key = kourier_db::DbKey::from([7; 32]);
    let mut conn = kourier_db::open(&dir.path().join("core.db"), &key).expect("open");
    kourier_db::migrate(
        &mut conn,
        &kourier_db::Migrations::new("core", &steps[..at]),
    )
    .expect("before");
    let has = |c: &Connection| {
        c.query_row(
            "SELECT count(*) FROM sqlite_master WHERE name = 'filters'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .expect("count")
    };
    assert_eq!(has(&conn), 0);
    kourier_db::migrate(&mut conn, &kourier_core::MIGRATIONS).expect("head");
    assert_eq!(has(&conn), 1);
    let columns: Vec<String> = conn
        .prepare("SELECT name FROM pragma_table_info('filters')")
        .expect("prepare")
        .query_map([], |r| r.get(0))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("rows");
    assert_eq!(
        columns,
        [
            "id",
            "position",
            "name",
            "text_pattern",
            "sender_pattern",
            "chat_pattern",
            "match_case",
            "notify",
            "notify_muted",
            "highlight",
            "colour",
            "created",
            "updated"
        ]
    );
    let insert = |p: i64| {
        conn.execute(
            "INSERT INTO filters(position, name, text_pattern, colour, created, updated)
             VALUES (?1, 'n', 'x', 0, 0, 0)",
            [p],
        )
    };
    insert(0).expect("first");
    let err = insert(0).expect_err("the position is unique");
    assert_eq!(
        err.sqlite_error_code(),
        Some(ErrorCode::ConstraintViolation)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn save_edit_move_delete_keep_the_positions_dense_and_move_the_revision() {
    let mut env = env().await;
    let l = list(&env).await;
    assert!(l.filters.is_empty() && l.set.is_empty());
    let r0 = l.revision;

    let r1 = save(&mut env, None, spec("one", Field::Text, "alpha"))
        .await
        .expect("one");
    let r2 = save(&mut env, None, spec("two", Field::Text, "beta"))
        .await
        .expect("two");
    let r3 = save(&mut env, None, spec("three", Field::Chat, "gamma"))
        .await
        .expect("three");
    assert!(
        r0 < r1 && r1 < r2 && r2 < r3,
        "every change moves the revision"
    );
    let l = list(&env).await;
    assert_eq!(l.revision, r3);
    assert_eq!(names(&l), ["one", "two", "three"]);
    assert_eq!(
        positions(&env).iter().map(|p| p.0).collect::<Vec<_>>(),
        [0, 1, 2]
    );
    let (a, b, c) = (l.filters[0].id, l.filters[1].id, l.filters[2].id);
    assert!(a != b && b != c);

    // An edit keeps the id and the place.
    let mut edited = spec("two!", Field::Text, "beta2");
    edited.notify = true;
    edited.notify_muted = true;
    let r4 = save(&mut env, Some(b), edited).await.expect("edit");
    assert!(r4 > r3);
    let l = list(&env).await;
    assert_eq!(names(&l), ["one", "two!", "three"]);
    assert_eq!(l.filters[1].id, b);
    assert!(l.filters[1].spec.notify && l.filters[1].spec.notify_muted);

    // Moves: to the front, past the end, to itself.
    command(
        &mut env,
        UiCommand::MoveFilter { id: c, to: 0 },
        "MoveFilter",
    )
    .await
    .expect("move");
    assert_eq!(names(&list(&env).await), ["three", "one", "two!"]);
    command(
        &mut env,
        UiCommand::MoveFilter { id: c, to: 99 },
        "MoveFilter",
    )
    .await
    .expect("move");
    assert_eq!(names(&list(&env).await), ["one", "two!", "three"]);
    let r = command(
        &mut env,
        UiCommand::MoveFilter { id: b, to: 1 },
        "MoveFilter",
    )
    .await
    .expect("move");
    assert_eq!(list(&env).await.revision, r);
    assert_eq!(
        positions(&env).iter().map(|p| p.0).collect::<Vec<_>>(),
        [0, 1, 2]
    );

    // A delete closes the gap; the set follows the list.
    command(&mut env, UiCommand::DeleteFilter { id: a }, "DeleteFilter")
        .await
        .expect("delete");
    let l = list(&env).await;
    assert_eq!(names(&l), ["two!", "three"]);
    assert_eq!(l.set.len(), 2);
    assert_eq!(
        positions(&env),
        [(0, "two!".to_owned()), (1, "three".to_owned())]
    );
    assert!(hits(&l, texts("alpha")).is_empty());
    assert_eq!(hits(&l, texts("some beta2")), [0]);
    assert_eq!(
        hits(
            &l,
            FilterTexts {
                text: None,
                sender: None,
                chat: Some("Gamma club")
            }
        ),
        [1]
    );
    env.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_refusal_is_typed_and_changes_nothing() {
    let mut env = env().await;
    let ok = save(&mut env, None, spec("keep", Field::Text, "x"))
        .await
        .expect("ok");
    let id = list(&env).await.filters[0].id;

    let mut no_field = spec("n", Field::Text, "x");
    no_field.text_pattern = None;
    let mut empty_pattern = spec("n", Field::Text, "");
    empty_pattern.chat_pattern = Some(Redacted::new(String::new()));
    let mut bad_colour = spec("n", Field::Text, "x");
    bad_colour.colour = 0x0100_0000;
    type Expect = Box<dyn Fn(&FilterRefusal) -> bool>;
    let cases: Vec<(FilterSpec, Expect)> = vec![
        (
            spec("  ", Field::Text, "x"),
            Box::new(|r| *r == FilterRefusal::NameEmpty),
        ),
        (
            spec(&"n".repeat(MAX_NAME_CHARS + 1), Field::Text, "x"),
            Box::new(|r| *r == FilterRefusal::NameTooLong),
        ),
        (no_field, Box::new(|r| *r == FilterRefusal::NoField)),
        (empty_pattern, Box::new(|r| *r == FilterRefusal::NoField)),
        (
            bad_colour,
            Box::new(|r| *r == FilterRefusal::ColourOutOfRange),
        ),
        (
            spec("n", Field::Sender, &"a".repeat(MAX_PATTERN_CHARS + 1)),
            Box::new(|r| *r == FilterRefusal::PatternTooLong(Field::Sender)),
        ),
        (
            spec("n", Field::Chat, "a(b"),
            Box::new(
                |r| matches!(r, FilterRefusal::Pattern(Field::Chat, e) if e.kind == PatternErrorKind::UnclosedGroup && e.at_utf16 == 1),
            ),
        ),
        (
            spec("n", Field::Text, "a(?=b)"),
            Box::new(
                |r| matches!(r, FilterRefusal::Pattern(Field::Text, e) if e.kind == PatternErrorKind::LookAroundUnsupported),
            ),
        ),
        (
            spec("n", Field::Text, r"(a)\1"),
            Box::new(
                |r| matches!(r, FilterRefusal::Pattern(_, e) if e.kind == PatternErrorKind::BackReferenceUnsupported),
            ),
        ),
        (
            spec("n", Field::Text, r"\p{L}{1000}"),
            Box::new(
                |r| matches!(r, FilterRefusal::Pattern(_, e) if e.kind == PatternErrorKind::TooLarge),
            ),
        ),
    ];
    for (s, expected) in cases {
        let got = save(&mut env, None, s.clone()).await.expect_err("refused");
        assert!(expected(&got), "{got:?}");
        // The same spec as an edit is refused the same way.
        let got = save(&mut env, Some(id), s).await.expect_err("refused");
        assert!(expected(&got), "{got:?}");
    }
    let gone = FilterId(id.0 + 1000);
    assert_eq!(
        save(&mut env, Some(gone), spec("n", Field::Text, "x")).await,
        Err(FilterRefusal::Unknown)
    );
    assert_eq!(
        command(
            &mut env,
            UiCommand::DeleteFilter { id: gone },
            "DeleteFilter"
        )
        .await,
        Err(FilterRefusal::Unknown)
    );
    assert_eq!(
        command(
            &mut env,
            UiCommand::MoveFilter { id: gone, to: 0 },
            "MoveFilter"
        )
        .await,
        Err(FilterRefusal::Unknown)
    );

    let l = list(&env).await;
    assert_eq!(l.revision, ok, "no refusal moved the revision");
    assert_eq!(names(&l), ["keep"]);
    // The limits themselves are allowed.
    save(
        &mut env,
        None,
        spec(
            &"n".repeat(MAX_NAME_CHARS),
            Field::Text,
            &format!("(?x){}a", " ".repeat(MAX_PATTERN_CHARS - 5)),
        ),
    )
    .await
    .expect("at the limits");
    env.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_set_answers_for_all_filled_fields_case_lines_and_cyrillic() {
    let mut env = env().await;
    let mut both = spec("both", Field::Text, "invoice");
    both.sender_pattern = Some(Redacted::new("^ali".into()));
    let mut exact = spec("exact", Field::Text, "Invoice");
    exact.match_case = true;
    for s in [
        both,
        exact,
        spec("lines", Field::Text, "^code:"),
        spec("cyr", Field::Text, r"\bдоставка\b"),
        spec("chat", Field::Chat, "^fam"),
    ] {
        save(&mut env, None, s).await.expect("save");
    }
    let l = list(&env).await;
    assert_eq!(hits(&l, texts("an Invoice")), [0, 1, 4]);
    assert_eq!(hits(&l, texts("an invoice")), [0, 4]);
    assert_eq!(
        hits(
            &l,
            FilterTexts {
                sender: Some("Bob"),
                ..texts("an invoice")
            }
        ),
        [4]
    );
    assert_eq!(hits(&l, texts("x\ncode: 1")), [2, 4]);
    assert_eq!(hits(&l, texts("Ваша Доставка тут")), [3, 4]);
    assert!(hits(&l, texts("Передоставка")).iter().all(|i| *i == 4));
    // The set is shared, not rebuilt per query.
    assert!(Arc::ptr_eq(&l.set, &list(&env).await.set));
    env.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_filters_survive_a_restart_and_the_muted_watch_follows_the_list() {
    let mut env = env().await;
    assert!(
        !env.core
            .as_ref()
            .expect("core")
            .filters_notify_muted_watch()
    );
    let mut watch = spec("w", Field::Text, "alpha");
    watch.notify = true;
    save(&mut env, None, watch.clone())
        .await
        .expect("notify only");
    assert!(
        !env.core
            .as_ref()
            .expect("core")
            .filters_notify_muted_watch(),
        "notify alone is no watch"
    );
    watch.notify_muted = true;
    let id = list(&env).await.filters[0].id;
    save(&mut env, Some(id), watch).await.expect("muted");
    assert!(
        env.core
            .as_ref()
            .expect("core")
            .filters_notify_muted_watch()
    );
    save(&mut env, None, spec("b", Field::Sender, "bob"))
        .await
        .expect("b");
    let before = list(&env).await;

    env.restart().await;
    assert!(
        env.core
            .as_ref()
            .expect("core")
            .filters_notify_muted_watch()
    );
    let after = list(&env).await;
    assert_eq!(after.filters, before.filters);
    assert_eq!(after.set.len(), 2);
    assert_eq!(hits(&after, texts("Alpha")), [0]);

    env.handle
        .send(UiCommand::DeleteFilter { id })
        .expect("send");
    // The start's own compile may have told the front-ends too (R6b): wait for the delete's
    // revision, not for that.
    let revision = after.revision;
    env.event("the delete", |e| {
        matches!(e, UiEvent::FiltersChanged { revision: r } if *r > revision).then_some(())
    })
    .await;
    assert!(
        !env.core
            .as_ref()
            .expect("core")
            .filters_notify_muted_watch()
    );
    env.stop().await;
}

fn set_start_delay(ms: u64) {
    kourier_core::filters::START_COMPILE_DELAY_MS.store(ms, std::sync::atomic::Ordering::SeqCst);
}

fn watch(env: &Env) -> bool {
    env.core
        .as_ref()
        .expect("core")
        .filters_notify_muted_watch()
}

/// R6b: the start does not wait for the compile; a query waits for it and answers the stored
/// filters with a revision of at least 1; the watches come from the stored rows at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_start_does_not_wait_for_the_compile_and_a_query_waits_for_it() {
    let mut env = env().await;
    let mut a = spec("a", Field::Text, "alpha");
    a.notify = true;
    a.notify_muted = true;
    save(&mut env, None, a).await.expect("a");
    save(&mut env, None, spec("b", Field::Sender, "bob"))
        .await
        .expect("b");
    set_start_delay(1500);
    let began = std::time::Instant::now();
    env.restart().await;
    let started = began.elapsed();
    assert!(
        started < std::time::Duration::from_millis(1200),
        "the start waited for the compile: {started:?}"
    );
    assert!(watch(&env), "the watch comes from the stored rows");
    let l = list(&env).await;
    assert!(began.elapsed() >= std::time::Duration::from_millis(1500));
    assert!(l.revision >= 1);
    assert_eq!(names(&l), ["a", "b"]);
    assert_eq!(l.set.len(), 2);
    assert_eq!(hits(&l, texts("alpha")), [0]);
    assert!(watch(&env));
    set_start_delay(0);
    env.stop().await;
}

/// R6b: a save sent right after the start (before the start's compile is done) wins: the
/// answer holds the saved list, never the start's older one, and so do the watches.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_save_before_the_start_compile_finishes_wins() {
    let mut env = env().await;
    let mut a = spec("a", Field::Text, "alpha");
    a.notify = true;
    a.notify_muted = true;
    save(&mut env, None, a).await.expect("a");
    set_start_delay(1500);
    env.restart().await;
    assert!(watch(&env));
    // Sent at once, not through `command`: its `list` would wait for the start's compile. The
    // restart subscribed anew and the start announces nothing once this save rebuilt the set, so
    // the first `FiltersChanged` is the save's.
    env.handle
        .send(UiCommand::SaveFilter {
            id: None,
            spec: spec("c", Field::Chat, "carol"),
        })
        .expect("send");
    outcome(&mut env, "SaveFilter", 0)
        .await
        .expect("saved during the start");
    let first = list(&env).await;
    assert_eq!(names(&first), ["a", "c"]);
    tokio::time::sleep(std::time::Duration::from_millis(2200)).await;
    let later = list(&env).await;
    assert_eq!(
        names(&later),
        ["a", "c"],
        "the start's older list must not replace it"
    );
    assert_eq!(later.revision, first.revision);
    assert!(watch(&env));
    set_start_delay(0);
    env.stop().await;
}

/// A filter whose name and pattern are unique markers is saved, listed, matched, edited,
/// refused, moved and deleted; nothing the core logs holds either.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_log_never_holds_a_pattern_or_a_name() {
    if support::in_own_process!() {
        return;
    }
    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for Sink {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("sink").extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let sink = Sink::default();
    let writer = sink.clone();
    tracing::subscriber::set_global_default(
        tracing_subscriber::registry().with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(move || writer.clone())
                .with_filter(tracing_subscriber::filter::LevelFilter::TRACE),
        ),
    )
    .expect("the only subscriber of this process");

    const PATTERN: &str = "zqxmarkerpattern7731";
    const NAME: &str = "zqxmarkername7731";
    let mut env = env().await;
    save(
        &mut env,
        None,
        spec(NAME, Field::Text, &format!("{PATTERN}\\d+")),
    )
    .await
    .expect("save");
    let l = list(&env).await;
    let id_cell = l.filters[0].id;
    assert_eq!(hits(&l, texts(&format!("{PATTERN}42"))), [0]);
    assert_eq!(
        l.set.spans(0, Field::Text, &format!("a {PATTERN}42")).len(),
        1
    );
    save(
        &mut env,
        Some(id_cell),
        spec(NAME, Field::Text, &format!("{PATTERN}(")),
    )
    .await
    .expect_err("refused");
    save(
        &mut env,
        Some(id_cell),
        spec(NAME, Field::Text, &format!(r"{PATTERN}(a)\1")),
    )
    .await
    .expect_err("refused");
    save(
        &mut env,
        Some(id_cell),
        spec(NAME, Field::Text, &format!(r"{PATTERN}\p{{L}}{{1000}}")),
    )
    .await
    .expect_err("refused");
    save(&mut env, None, spec("other", Field::Chat, "x"))
        .await
        .expect("second");
    command(
        &mut env,
        UiCommand::MoveFilter { id: id_cell, to: 1 },
        "MoveFilter",
    )
    .await
    .expect("move");
    command(
        &mut env,
        UiCommand::DeleteFilter { id: id_cell },
        "DeleteFilter",
    )
    .await
    .expect("delete");
    env.restart().await;
    env.stop().await;

    let log = String::from_utf8_lossy(&sink.0.lock().expect("sink")).into_owned();
    assert!(
        log.contains("filters changed"),
        "the core logged the change (counts only):\n{log}"
    );
    assert!(
        !log.contains("zqxmarker"),
        "a pattern or a name reached the log"
    );
    let _: Option<Filter> = None;
}

/// R4 item 3: the catch-up summary carries the ids of the newest 2,000 caught-up messages,
/// oldest first, while some filter has Notify; with no Notify filter it carries none.
async fn summary_ids(with_notify: bool) -> Vec<i64> {
    use kourier_backend_api::BackendEvent;
    use kourier_fake::Step;
    use kourier_fake::scripts::{T0, remote};
    use kourier_model::ConnState;
    use support::send::{self, d_alice};

    let events: Vec<BackendEvent> = (1..=2_001_i64)
        .map(|n| {
            let mut m = remote(&d_alice(), &send::alice(), &n.to_string(), T0 + n, "away");
            m.catch_up = true;
            BackendEvent::Message(m)
        })
        .collect();
    let s = send::script("catch-up-cap")
        .step(Step::Conn(ConnState::Syncing { fraction: None }))
        .deliver(events)
        .step(Step::CatchUpDone);
    let mut env = Env::new(Flavor::SignalLike, s).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    if with_notify {
        let mut f = spec("n", Field::Text, "away");
        f.notify = true;
        save(&mut env, None, f).await.expect("save");
    }
    let account = support::add_linked(&mut env).await;
    let ids = env
        .event("the summary", |e| match e {
            UiEvent::CatchUpSummary {
                account: a, ids, ..
            } if *a == account => Some(ids.clone()),
            _ => None,
        })
        .await;
    let sort_keys: Vec<i64> = {
        let msgs: Vec<i64> = ids.iter().map(|(_, m)| m.0).collect();
        msgs
    };
    env.stop().await;
    sort_keys
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_catch_up_summary_keeps_the_newest_2000_ids_only_with_a_notify_filter() {
    let ids = summary_ids(true).await;
    assert_eq!(ids.len(), kourier_core::facade::CATCH_UP_IDS);
    assert!(ids.windows(2).all(|w| w[0] < w[1]), "oldest first");
    // 2,001 arrived: the oldest one is the one dropped.
    let all_first = ids[0];
    assert_eq!(ids[ids.len() - 1] - all_first, 1_999, "consecutive newest");
    assert!(
        summary_ids(false).await.is_empty(),
        "no Notify filter: none"
    );
}
