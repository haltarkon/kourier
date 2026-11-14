//! The words of deleted messages leave the FILE (G1d; critic H4, H6, H2; decisions L2, M3,
//! M7), after the fix round (the index in its own file, search.db): every page of search.db and
//! every frame of its WAL is decrypted (`support::pages`) and searched for the TOKEN of each
//! word; core.db, which holds the bodies, never holds a token at all. Each raw word is typed so
//! its token is bytes the message body does not hold (upper case, an apostrophe: `М'ЯТАXQ` is
//! stored as typed, its token `мятаxq` only by the index), so a page that holds a token before
//! the compaction can only be the index's: the look is not vacuous. Each token starts with a
//! byte no other term of the index has, because FTS5 stores a term after the bytes it shares
//! with the one before it. The index is larger than one step of the compaction (over 64 leaf
//! pages) and the deleted rows are far under FTS5's own 10 % that would merge a segment by
//! itself. The text is synthetic.

use crate::support;

use std::path::Path;
use std::time::Duration;

use kourier_backend_api::BackendEvent;
use kourier_core::facade::UiCommand;
use kourier_core::testing::{IngestHarness, search};
use kourier_fake::scripts::{T0, remote, target};
use kourier_fake::{Flavor, Script};
use kourier_model::{
    Attachment, AttachmentKind, Body, ConvKey, ConversationId, MessageId, MsgIdScope, NetworkKind,
    Text, UserId,
};

use support::pages::{RawLook, look_at};
use support::{CORE_KEY, Env, SEARCH_KEY, WAIT, add_linked, eventually};

const ME: &str = "aci:00000000-0000-4000-8000-0000000000aa";
const ALICE: &str = "aci:00000000-0000-4000-8000-0000000a11ce";
const BOB: &str = "aci:00000000-0000-4000-8000-0000000b0b00";
const DAVE: &str = "aci:00000000-0000-4000-8000-0000000da7e0";

const KEY: [u8; 32] = [7; 32];
/// A text of search.db's schema in page 1: the decryption gives plaintext.
const PROOF_SEARCH: &[u8] = b"messages_fts";
/// A text of core.db's schema in page 1.
const PROOF_CORE: &[u8] = b"search_queue";
/// Filler rows: enough for well over 64 leaf pages of the index (three long words each).
const FILLERS: i64 = 10_000;
/// A day of the core's clock, in ms.
const DAY: i64 = 24 * 3600 * 1000;

fn uid(s: &str) -> UserId {
    UserId::parse(s).expect("user id")
}

fn chat(who: &str) -> ConvKey {
    ConvKey::Direct(uid(who))
}

fn said(who: &str, local: &str, n: i64, text: &str) -> BackendEvent {
    BackendEvent::Message(remote(&chat(who), &uid(who), local, T0 + n * 1000, text))
}

fn conv_of(conn: &kourier_db::rusqlite::Connection, who: &str) -> ConversationId {
    conn.query_row(
        "SELECT id FROM conversations WHERE conv_key = ?1",
        [chat(who).encode()],
        |r| r.get(0).map(ConversationId),
    )
    .expect("conversation")
}

fn id_of(conn: &kourier_db::rusqlite::Connection, local: &str) -> MessageId {
    conn.query_row(
        "SELECT id FROM messages WHERE net_local = ?1",
        [local],
        |r| r.get(0).map(MessageId),
    )
    .expect("message")
}

/// The text of filler row `i`: three long words that start with `f`, `g` and `k` only, so
/// they share no first byte with a token looked for, and few bytes with each other.
fn filler(i: i64) -> String {
    let h = |k: u64| {
        (u64::try_from(i).expect("positive").wrapping_add(k)).wrapping_mul(0x9e37_79b9_7f4a_7c15)
            >> 16
    };
    format!("f{:012x}q g{:012x}h k{:012x}m", h(1), h(2), h(3))
}

/// Filler rows `range` of chat `conv` written straight into `messages` (the triggers log
/// them), with the words of [`filler`].
fn fill(
    conn: &kourier_db::rusqlite::Connection,
    account: i64,
    conv: ConversationId,
    author: &str,
    range: std::ops::Range<i64>,
) {
    let tx = conn.unchecked_transaction().expect("transaction");
    {
        let mut insert = tx
            .prepare(
                "INSERT INTO messages(account_id, conversation_id, net_scope, net_author, net_local,
                     origin, sort_key, sent_at, received_at, body_kind, body)
                   VALUES (?1, ?2, 0, ?3, ?4, 'Remote', ?5 * 1024, ?5, ?5, 'Text', ?6)",
            )
            .expect("prepare");
        for i in range {
            let body = format!("{{\"Text\":{{\"body\":\"{}\"}}}}", filler(i));
            insert
                .execute((
                    account,
                    conv.0,
                    author,
                    format!("f{i}"),
                    T0 - 1_000_000 + i,
                    body,
                ))
                .expect("filler");
        }
    }
    tx.commit().expect("commit");
}

/// The raw look at `db` (search.db or core.db, with `proof` of its schema) and its WAL.
fn look(db: &Path, key: &[u8; 32], tokens: &[&str], proof: &[u8]) -> RawLook {
    let needles: Vec<&[u8]> = tokens.iter().map(|t| t.as_bytes()).collect();
    let found = look_at(db, key, &needles, proof);
    assert!(found.with_proof > 0, "the pages decrypt to plaintext");
    found
}

fn token(word: &str) -> String {
    search::token_of(word)
}

fn meta(search: &kourier_db::rusqlite::Connection, key: &str) -> i64 {
    search::meta(search, key).expect("meta")
}

/// Each path of G1b that deletes a message, on one profile: the token of each deleted word is
/// in the index's pages of search.db before, and in no page of search.db and no frame of its WAL
/// after the compaction and its truncating checkpoint; a view-once row's token is in no page
/// ever (critic H2); core.db holds no token at any time (the index is search.db's alone). The
/// WAL of search.db is never checkpointed by itself here (`wal_autocheckpoint = 0`): every page
/// image written stays in it until the truncating checkpoint, the worst case. A reader that
/// holds a snapshot of search.db keeps the checkpoint from truncating until it ends.
#[test]
fn the_words_of_every_deleted_message_leave_the_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("core.db");
    let mut h = IngestHarness::open(
        &db,
        &kourier_db::DbKey::from(KEY),
        NetworkKind::Fake,
        MsgIdScope::AccountAuthor,
        &[uid(ME)],
    )
    .expect("harness");
    let sdb = h.search_path().to_path_buf();
    h.search_db()
        .execute_batch("PRAGMA wal_autocheckpoint = 0")
        .expect("no automatic checkpoint");
    // The words, as typed, and the path that deletes each.
    let words = [
        ("М'ЯТАXQ", "delete for everyone"),
        ("ZEBRAXQ", "delete for me from another device"),
        ("YAKXQ", "expiry"),
        ("WOLFXQ", "clear"),
        ("VOLEXQ", "chat delete on the phone"),
    ];
    let once = "UNICORNXQ";
    let tokens: Vec<String> = words.iter().map(|(w, _)| token(w)).collect();
    assert_eq!(tokens[0], "мятаxq");
    let once_token = token(once);
    let mut expiring = remote(&chat(ALICE), &uid(ALICE), "a3", T0 + 3000, words[2].0);
    expiring.expiry = Some(kourier_model::Expiry {
        kind: kourier_model::DisappearKind::AfterSend,
        timer: Duration::from_secs(60),
        started_at: Some(T0 + 3000),
    });
    let mut view_once = remote(&chat(ALICE), &uid(ALICE), "a4", T0 + 4000, "");
    view_once.body = Body::Media {
        attachments: vec![Attachment {
            kind: AttachmentKind::Image,
            mime: "image/jpeg".into(),
            size: 10,
            file_name: Some(format!("{once}.jpg")),
            ..Attachment::default()
        }],
        caption: Some(Text::plain(once)),
    };
    view_once.view_once = true;
    h.apply(vec![said(ALICE, "a0", 0, "alicefirstq")])
        .expect("apply");
    let alice = conv_of(h.connection(), ALICE);
    fill(h.connection(), h.account().0, alice, ALICE, 0..FILLERS / 2);
    h.apply(vec![
        said(ALICE, "a1", 1, words[0].0),
        said(ALICE, "a2", 2, words[1].0),
        BackendEvent::Message(expiring),
        BackendEvent::Message(view_once),
        said(BOB, "b1", 5, words[3].0),
        said(BOB, "b2", 6, "bobotherq"),
        said(DAVE, "d1", 7, words[4].0),
        said(DAVE, "d2", 8, "daveotherq"),
    ])
    .expect("apply");
    fill(
        h.connection(),
        h.account().0,
        alice,
        ALICE,
        FILLERS / 2..FILLERS,
    );
    h.search_flush().expect("flush");
    let leaves: i64 = h
        .search_db()
        .query_row("SELECT count(*) FROM messages_fts_data", [], |r| r.get(0))
        .expect("leaves");
    eprintln!("search_attack: the index has {leaves} pages of data");
    assert!(
        leaves > 64,
        "the index is larger than one step: {leaves} pages"
    );
    let all: Vec<&str> = tokens
        .iter()
        .map(String::as_str)
        .chain([once_token.as_str()])
        .collect();
    let before = look(&sdb, &KEY, &all, PROOF_SEARCH);
    for (n, (_, path)) in words.iter().enumerate() {
        assert!(
            before.with[n] > 0,
            "{path}: the token is in the index's pages before"
        );
    }
    assert_eq!(before.with[5], 0, "a view-once row's token is in no page");
    assert_eq!(
        look(&db, &KEY, &all, PROOF_CORE).with,
        vec![0; 6],
        "core.db holds no token of the index"
    );
    assert!(
        search::ids_with(h.search_db(), once)
            .expect("raw")
            .is_empty()
    );

    // Every path deletes its message.
    h.apply(vec![
        BackendEvent::Deleted {
            target: target(Some(&chat(ALICE)), Some(&uid(ALICE)), "a1"),
            by: Some(uid(ALICE)),
            for_everyone: true,
            at: T0 + 20_000,
        },
        BackendEvent::Deleted {
            target: target(Some(&chat(ALICE)), Some(&uid(ALICE)), "a2"),
            by: None,
            for_everyone: false,
            at: T0 + 20_000,
        },
    ])
    .expect("deletes");
    h.set_now(T0 + 3000 + 61_000);
    h.sweep().expect("expiry");
    let bob = conv_of(h.connection(), BOB);
    h.clear_conversation(bob, false).expect("clear");
    h.apply(vec![BackendEvent::ConversationRemoved { conv: chat(DAVE) }])
        .expect("chat removed");
    for (n, (_, path)) in words.iter().enumerate() {
        assert_eq!(
            search::ids_with(h.search_db(), &tokens[n])
                .expect("raw")
                .len(),
            1,
            "{path}: still indexed until the flush"
        );
    }
    let flushed = h.search_flush().expect("flush");
    assert!(flushed.removed >= 5, "{flushed:?}");
    let dead = meta(h.search_db(), "dead");
    assert!(dead >= 5);
    let asked = search::compact_after(h.connection()).expect("compact_after");
    assert!(
        asked > meta(h.search_db(), "compacted_for") && meta(h.search_db(), "applied") >= asked,
        "a clear and a chat delete ask for a compaction at once, and the flush reached the ask"
    );
    for (n, (_, path)) in words.iter().enumerate() {
        assert!(
            search::ids_with(h.search_db(), &tokens[n])
                .expect("raw")
                .is_empty(),
            "{path}: out of the index"
        );
    }
    let tombstoned = look(&sdb, &KEY, &all, PROOF_SEARCH);
    for (n, (_, path)) in words.iter().enumerate() {
        assert!(
            tombstoned.with[n] > 0,
            "{path}: a tombstone leaves the token's bytes in the file"
        );
    }
    // A reader that holds a snapshot of search.db keeps the checkpoint from truncating.
    let reader = kourier_db::open_read_only(&sdb, &kourier_db::DbKey::from(KEY)).expect("reader");
    reader.execute_batch("BEGIN").expect("begin");
    let _: i64 = reader
        .query_row("SELECT count(*) FROM messages_fts_docsize", [], |r| {
            r.get(0)
        })
        .expect("snapshot");
    assert_eq!(
        search::due(h.search_db(), asked, T0).expect("due"),
        search::Due::Now
    );
    let compacted = search::compact_all(h.search_writer(), asked, T0).expect("compact");
    assert!(compacted.steps > 1, "{compacted:?}");
    assert!(!compacted.checkpointed, "a reader held the checkpoint");
    assert_eq!(
        meta(h.search_db(), "checkpoint_due"),
        1,
        "the checkpoint stays due while it is refused"
    );
    let held = look(&sdb, &KEY, &all, PROOF_SEARCH);
    assert!(
        held.with.iter().take(5).any(|n| *n > 0),
        "the WAL still holds the old pages while the reader holds them"
    );
    reader.execute_batch("COMMIT").expect("end");
    drop(reader);
    assert!(
        search::checkpoint(h.search_db()).expect("checkpoint"),
        "asked again"
    );
    let after = look(&sdb, &KEY, &all, PROOF_SEARCH);
    eprintln!(
        "search_attack: {} pages; tokens before {:?}, after the tombstones {:?}, after the \
         compaction ({} steps) {:?}",
        after.pages, before.with, tombstoned.with, compacted.steps, after.with
    );
    assert_eq!(
        after.with,
        vec![0; 6],
        "no token of a deleted word is in the file"
    );
    assert_eq!(meta(h.search_db(), "dead"), 0);
    assert_eq!(meta(h.search_db(), "compacted_for"), asked);
    assert_eq!(meta(h.search_db(), "checkpoint_due"), 0);
    assert_eq!(
        look(&db, &KEY, &all, PROOF_CORE).with,
        vec![0; 6],
        "core.db holds no token of the index"
    );
    search::integrity_check(h.search_db()).expect("integrity-check");
    assert_eq!(
        search::ids_with(h.search_db(), &filler(17)[..14])
            .expect("raw")
            .len(),
        1,
        "the rest is still found"
    );
}

/// The ask and the snapshot rule (critic H6): a clear whose log is larger than one flush asks
/// for a compaction, which is not due until `applied` has reached the ask; a tombstone written
/// while it runs (the task never flushes between two steps; the harness does, as a writer that
/// would) stays counted (`dead` is reduced by the snapshot, never set to 0), and the 24 hour
/// rule takes it from the compaction's start, out of the file too.
#[test]
fn a_compaction_waits_for_the_log_and_keeps_what_came_meanwhile() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("core.db");
    let mut h = IngestHarness::open(
        &db,
        &kourier_db::DbKey::from(KEY),
        NetworkKind::Fake,
        MsgIdScope::AccountAuthor,
        &[uid(ME)],
    )
    .expect("harness");
    h.apply(vec![
        said(BOB, "b0", 0, "QUOKKAXQ"),
        said(ALICE, "a0", 1, "alicefirstq"),
        said(ALICE, "a1", 2, "TAPIRXQ"),
    ])
    .expect("apply");
    let bob = conv_of(h.connection(), BOB);
    fill(h.connection(), h.account().0, bob, BOB, 0..1_200);
    h.search_flush().expect("flush");
    let quokka = token("QUOKKAXQ");
    let tapir = token("TAPIRXQ");
    h.clear_conversation(bob, false).expect("clear");
    assert_eq!(search::queued(h.connection()).expect("log"), 1_201);
    let asked = search::compact_after(h.connection()).expect("compact_after");
    assert_eq!(
        asked,
        search::position(h.connection()).expect("position"),
        "the ask names the clear's last log row"
    );
    // Flush by flush, as the task's jobs: no compaction until `applied` reached the ask.
    let mut flushes = 0;
    loop {
        assert_ne!(
            search::due(h.search_db(), asked, T0).expect("due"),
            search::Due::Now,
            "due before the log drained (flush {flushes})"
        );
        let one = h.search_flush_one(500).expect("one flush");
        flushes += 1;
        if !one.more {
            break;
        }
    }
    assert!(flushes >= 3, "the log was larger than one flush");
    assert_eq!(
        search::due(h.search_db(), asked, T0).expect("due"),
        search::Due::Now
    );
    let snapshot = search::begin(h.search_db()).expect("begin");
    assert_eq!(snapshot, 1_201);
    search::step(h.search_writer()).expect("a step");
    // A delete comes while it runs.
    h.apply(vec![BackendEvent::Deleted {
        target: target(Some(&chat(ALICE)), Some(&uid(ALICE)), "a1"),
        by: Some(uid(ALICE)),
        for_everyone: true,
        at: T0 + 20_000,
    }])
    .expect("delete");
    h.search_flush().expect("flush");
    assert_eq!(meta(h.search_db(), "dead"), 1_202);
    while search::step(h.search_writer()).expect("a step") {}
    search::finish(h.search_writer(), snapshot, asked, T0).expect("finish");
    assert_eq!(
        meta(h.search_db(), "dead"),
        1,
        "reduced by the snapshot, not set to 0"
    );
    assert_eq!(meta(h.search_db(), "compacted_for"), asked);
    assert_eq!(
        meta(h.search_db(), "dead_since"),
        T0,
        "what came meanwhile counts from the compaction's start"
    );
    assert!(search::checkpoint(h.search_db()).expect("checkpoint"));
    // The next one takes the rest, by the day rule (the ask is answered).
    assert_eq!(
        search::due(h.search_db(), asked, T0 + DAY - 1).expect("due"),
        search::Due::Not { at: Some(T0 + DAY) }
    );
    assert_eq!(
        search::due(h.search_db(), asked, T0 + DAY).expect("due"),
        search::Due::Now
    );
    let done = search::compact_all(h.search_writer(), asked, T0 + DAY).expect("compact");
    assert!(done.checkpointed);
    assert_eq!(meta(h.search_db(), "dead"), 0);
    assert_eq!(meta(h.search_db(), "dead_since"), 0);
    let after = look(
        h.search_path(),
        &KEY,
        &[quokka.as_str(), tapir.as_str()],
        PROOF_SEARCH,
    );
    assert_eq!(after.with, [0, 0], "both left the file");
}

/// The 24 hour rule holds while the backfill runs (decisions L5: a compaction then would fold
/// every segment again and again); one asked for at once still runs.
#[test]
fn the_day_rule_waits_for_the_backfill_and_an_ask_does_not() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = IngestHarness::open(
        &dir.path().join("core.db"),
        &kourier_db::DbKey::from(KEY),
        NetworkKind::Fake,
        MsgIdScope::AccountAuthor,
        &[uid(ME)],
    )
    .expect("harness");
    let mut events = Vec::new();
    for n in 1..=40 {
        events.push(said(ALICE, &format!("a{n}"), n, &format!("row{n}q")));
    }
    h.apply(events).expect("apply");
    h.search_rebuild().expect("as a file built again");
    h.search_backfill_one(10).expect("one batch");
    // A row the backfill wrote is deleted.
    h.apply(vec![BackendEvent::Deleted {
        target: target(Some(&chat(ALICE)), Some(&uid(ALICE)), "a40"),
        by: Some(uid(ALICE)),
        for_everyone: true,
        at: T0 + 50_000,
    }])
    .expect("delete");
    h.search_flush().expect("flush");
    assert_eq!(meta(h.search_db(), "dead"), 1);
    let asked = search::compact_after(h.connection()).expect("compact_after");
    assert_eq!(
        search::due(h.search_db(), asked, T0).expect("due"),
        search::Due::Not { at: None },
        "no day's deadline while the backfill runs"
    );
    assert_eq!(
        search::due(h.search_db(), asked, T0 + 2 * DAY).expect("due"),
        search::Due::Not { at: None }
    );
    h.search_backfill_all(10).expect("backfill");
    assert_eq!(
        search::due(h.search_db(), asked, T0 + DAY - 1).expect("due"),
        search::Due::Not { at: Some(T0 + DAY) },
        "the day counts from the first time the tombstone was seen"
    );
    assert_eq!(
        search::due(h.search_db(), asked, T0 + DAY).expect("due"),
        search::Due::Now
    );
    // A `dead_since` ahead of the core's clock (a clock set back) counts from the clock's now,
    // never from the future (privacy LOW-6).
    search::set_meta(h.search_db(), "dead_since", T0 + 10 * DAY).expect("ahead");
    assert_eq!(
        search::due(h.search_db(), asked, T0 + DAY).expect("due"),
        search::Due::Not {
            at: Some(T0 + 2 * DAY)
        },
        "the day counts from the clock's now"
    );
    assert_eq!(meta(h.search_db(), "dead_since"), T0 + DAY);
    // An ask runs during a backfill too.
    h.search_rebuild().expect("again");
    assert_eq!(
        meta(h.search_db(), "dead"),
        0,
        "a new file has no tombstone"
    );
    h.search_backfill_one(10).expect("one batch");
    h.apply(vec![BackendEvent::Deleted {
        target: target(Some(&chat(ALICE)), Some(&uid(ALICE)), "a39"),
        by: Some(uid(ALICE)),
        for_everyone: true,
        at: T0 + 60_000,
    }])
    .expect("delete");
    let alice = conv_of(h.connection(), ALICE);
    h.clear_conversation(alice, false).expect("clear");
    h.search_flush().expect("flush");
    assert!(meta(h.search_db(), "floor") > 0);
    let asked = search::compact_after(h.connection()).expect("compact_after");
    assert_eq!(
        search::due(h.search_db(), asked, T0).expect("due"),
        search::Due::Now
    );
}

fn writable(env: &Env) -> kourier_db::rusqlite::Connection {
    kourier_db::open(&env.core_db(), &kourier_db::DbKey::from(CORE_KEY)).expect("open")
}

/// A number of search.db's `meta`, or -1 while the file is not there with its schema yet (the
/// search task makes it at its start; a look can come between the file and its schema).
fn meta_of(env: &Env, key: &'static str) -> i64 {
    kourier_db::open_read_only(&env.search_db(), &kourier_db::DbKey::from(SEARCH_KEY))
        .ok()
        .and_then(|c| search::meta(&c, key).ok())
        .unwrap_or(-1)
}

/// Through the running core, on its clock (decisions M3, M7): the words of a single delete
/// stay in search.db for the day his answer allows and leave it then, with the index well
/// larger than one step of the compaction, so a compaction that stops early leaves them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_single_delete_leaves_the_file_within_a_day_of_the_core_clock() {
    let script = Script::new("search", vec![uid(ME)]).deliver(vec![said(ALICE, "s1", 1, "startq")]);
    let mut env = Env::new(Flavor::SignalLike, script).await;
    let account = add_linked(&mut env).await;
    env.wait_finished(account).await;
    env.stop().await;
    let needle = token("ZEBRAXQ");
    let target_id = {
        let conn = writable(&env);
        let alice = conv_of(&conn, ALICE);
        fill(&conn, account.0, alice, ALICE, 0..FILLERS);
        conn.execute(
            "INSERT INTO messages(account_id, conversation_id, net_scope, net_author, net_local,
                 origin, sort_key, sent_at, received_at, body_kind, body)
               VALUES (?1, ?2, 0, ?3, 'needle', 'Remote', ?4 * 1024, ?4, ?4, 'Text',
                 '{\"Text\":{\"body\":\"ZEBRAXQ\"}}')",
            (account.0, alice.0, ALICE, T0),
        )
        .expect("needle");
        id_of(&conn, "needle")
    };
    env.restart().await;
    env.handle.send(UiCommand::UiReady).expect("send");
    // search.db is made at UiReady with every row below its floor: the backfill writes them.
    eventually("everything is indexed", Duration::from_secs(120), || {
        let done = meta_of(&env, "floor") == 0
            && env.peek_search(|c| search::ids_with(c, "ZEBRAXQ").expect("raw").len()) == 1;
        async move { done }
    })
    .await;
    let sdb = env.search_db();
    let before = look(&sdb, &SEARCH_KEY, &[needle.as_str()], PROOF_SEARCH);
    assert!(before.with[0] > 0, "the token is in the index's pages");
    env.handle
        .send(UiCommand::DeleteMessage {
            message: target_id,
            for_everyone: false,
        })
        .expect("send");
    eventually("the delete reaches the index", WAIT, || {
        let dead = meta_of(&env, "dead");
        async move { dead == 1 }
    })
    .await;
    assert!(
        env.peek_search(|c| search::ids_with(c, "ZEBRAXQ").expect("raw"))
            .is_empty()
    );
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(
        meta_of(&env, "dead"),
        1,
        "a single delete waits for the day"
    );
    assert!(meta_of(&env, "dead_since") > 0);
    env.clock.advance(Duration::from_secs(24 * 3600 + 1));
    eventually(
        "the compaction after a day",
        Duration::from_secs(120),
        || {
            let dead = meta_of(&env, "dead");
            let since = meta_of(&env, "dead_since");
            async move { dead == 0 && since == 0 }
        },
    )
    .await;
    // Its truncating checkpoint comes right after its last step.
    let after = {
        let mut last = look(&sdb, &SEARCH_KEY, &[needle.as_str()], PROOF_SEARCH);
        let started = std::time::Instant::now();
        while last.with[0] > 0 && started.elapsed() < WAIT {
            tokio::time::sleep(Duration::from_millis(100)).await;
            last = look(&sdb, &SEARCH_KEY, &[needle.as_str()], PROOF_SEARCH);
        }
        last
    };
    eprintln!(
        "search_attack: one delete in {} pages: the token before {:?}, after the day {:?}",
        after.pages, before.with, after.with
    );
    assert_eq!(after.with, [0], "the token left the file");
    assert_eq!(
        env.peek_search(|c| search::ids_with(c, &filler(17)[..14]).expect("raw").len()),
        1
    );
    env.stop().await;
}

/// An account's removal asks for the compaction at once: its words leave search.db after the
/// log has drained, with no clock moved.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_account_removal_takes_its_words_out_of_the_file_at_once() {
    let script = Script::new("search", vec![uid(ME)]).deliver(vec![
        said(ALICE, "s1", 1, "М'ЯТАXQ"),
        said(ALICE, "s2", 2, "otherwordq"),
    ]);
    let mut env = Env::new(Flavor::SignalLike, script).await;
    let account = add_linked(&mut env).await;
    env.wait_finished(account).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    let needle = token("М'ЯТАXQ");
    // The rows came before search.db: the backfill writes them after its delay.
    eventually("indexed", WAIT * 2, || {
        // search.db may exist a moment before its schema is committed.
        let n = env.search_db().exists()
            && env
                .try_peek_search(|c| search::ids_with(c, "М'ЯТАXQ"))
                .is_some_and(|ids| ids.len() == 1);
        async move { n }
    })
    .await;
    let sdb = env.search_db();
    assert!(look(&sdb, &SEARCH_KEY, &[needle.as_str()], PROOF_SEARCH).with[0] > 0);
    env.handle
        .send(UiCommand::RemoveAccount { account })
        .expect("send");
    eventually("compacted at once", WAIT, || {
        let indexed = env.peek_search(|c| search::indexed(c).expect("docsize").len());
        let dead = meta_of(&env, "dead");
        let asked = env.peek(|c| search::compact_after(c).expect("compact_after"));
        let answered = meta_of(&env, "compacted_for");
        let gone = look(&sdb, &SEARCH_KEY, &[needle.as_str()], PROOF_SEARCH).with[0] == 0;
        async move { indexed == 0 && dead == 0 && asked > 0 && answered >= asked && gone }
    })
    .await;
    env.stop().await;
}
