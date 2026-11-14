//! The backfill of the search index (G1c): the rows that were there before search.db (its
//! first start over a core.db with rows, or every row after a rebuild: new token rules, a file
//! that does not belong) are written into `messages_fts` newest id first, in batches whose
//! cursor (`floor`, in search.db's `meta`) moves in the batch's own transaction. Every assertion looks at the INDEX ITSELF (`testing::search`, critic H2). The
//! kill loop's oracle is critic H3's: a row of the killed batch is edited before the restart,
//! and its old text must not be findable afterwards (a duplicate entry would keep it for
//! good). The text is synthetic; each word is one no other row holds.

use crate::support;

use std::collections::BTreeMap;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use kourier_backend_api::{BackendEvent, IncomingMessage, NetworkRegistry};
use kourier_core::Timeouts;
use kourier_core::facade::UiCommand;
use kourier_core::testing::{IngestHarness, search};
use kourier_fake::scripts::{T0, remote};
use kourier_fake::{Flavor, Script};
use kourier_model::{
    Body, ConvKey, ConversationId, MessageId, MsgIdScope, NetworkKind, SystemEvent, Text, UserId,
};

use support::{Env, SEARCH_KEY, WAIT, add_linked, eventually};

const ME: &str = "aci:00000000-0000-4000-8000-0000000000aa";
const ALICE: &str = "aci:00000000-0000-4000-8000-0000000a11ce";
const CAROL: &str = "aci:00000000-0000-4000-8000-0000000ca202";

const KEY: [u8; 32] = [5; 32];
const CHILD_DB: &str = "KOURIER_TEST_SEARCH_BACKFILL_DB";
const SIGABRT: i32 = 6;
const CHILD_TIMEOUT: Duration = Duration::from_secs(120);
/// The kill loop's batch: 140 rows give 20 batches, so 20 kills.
const BATCH: usize = 7;

fn uid(s: &str) -> UserId {
    UserId::parse(s).expect("user id")
}

fn chat(who: &str) -> ConvKey {
    ConvKey::Direct(uid(who))
}

fn harness(path: &Path) -> IngestHarness {
    IngestHarness::open(
        path,
        &kourier_db::DbKey::from(KEY),
        NetworkKind::Fake,
        MsgIdScope::AccountAuthor,
        &[uid(ME)],
    )
    .expect("harness")
}

fn text_body(word: &str) -> String {
    format!("{{\"Text\":{{\"body\":\"{word}\"}}}}")
}

/// Who wrote row `n`: Alice in her chat (odd) or Carol in hers (even).
fn who(n: i64) -> &'static str {
    if n % 2 == 1 { ALICE } else { CAROL }
}

/// Row `n` (see [`who`]).
fn row(n: i64, text: &str) -> IncomingMessage {
    remote(
        &chat(who(n)),
        &uid(who(n)),
        &n.to_string(),
        T0 + n * 1000,
        text,
    )
}

/// `n` rows in two chats: a text with a word of its own, every 9th view-once, every 11th a
/// notice whose title holds a word, every 17th a body that does not parse; then the log is
/// flushed and search.db rebuilt, so every row waits below `floor` as in the file of a first
/// start. Returns the words of the rows the index must hold.
fn seed(h: &mut IngestHarness, n: i64) -> BTreeMap<MessageId, String> {
    let mut events = Vec::new();
    for i in 1..=n {
        let word = format!("fill{i}q");
        let mut m = row(i, &format!("{word} common"));
        if i % 9 == 0 {
            m.view_once = true;
        } else if i % 11 == 0 {
            m.body = Body::System(SystemEvent::TitleChanged {
                by: Some(uid(who(i))),
                title: word.clone(),
            });
        }
        events.push(BackendEvent::Message(m));
    }
    h.apply(events).expect("apply");
    let ids = ids_desc(h.connection());
    assert_eq!(ids.len(), usize::try_from(n).expect("small"));
    let mut words = BTreeMap::new();
    for &id in &ids {
        let i = local_of(h.connection(), id);
        if i % 17 == 0 && i % 9 != 0 && i % 11 != 0 {
            h.connection()
                .execute("UPDATE messages SET body = '{}' WHERE id = ?1", [id])
                .expect("poison");
        } else if i % 9 != 0 && i % 11 != 0 {
            words.insert(MessageId(id), format!("fill{i}q"));
        }
    }
    h.search_flush().expect("flush");
    h.search_rebuild().expect("rebuild");
    assert_eq!(meta(h.search_db(), "floor"), ids[0]);
    assert!(search::indexed(h.search_db()).expect("docsize").is_empty());
    words
}

fn ids_desc(conn: &kourier_db::rusqlite::Connection) -> Vec<i64> {
    conn.prepare("SELECT id FROM messages ORDER BY id DESC")
        .expect("prepare")
        .query_map([], |r| r.get(0))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("ids")
}

fn local_of(conn: &kourier_db::rusqlite::Connection, id: i64) -> i64 {
    let local: String = conn
        .query_row("SELECT net_local FROM messages WHERE id = ?1", [id], |r| {
            r.get(0)
        })
        .expect("local");
    local.parse().expect("number")
}

fn conv_of(conn: &kourier_db::rusqlite::Connection, who: &str) -> ConversationId {
    conn.query_row(
        "SELECT id FROM conversations WHERE conv_key = ?1",
        [chat(who).encode()],
        |r| r.get(0).map(ConversationId),
    )
    .expect("conversation")
}

fn meta(search: &kourier_db::rusqlite::Connection, key: &str) -> i64 {
    search::meta(search, key).expect("meta")
}

/// The index holds exactly `words`: one entry per row (`%_docsize`), each row found by its
/// word and by nothing else's, the `indexed` map names exactly those entries, FTS5's own
/// check passes, the log is empty and the backfill is over.
fn assert_whole(h: &IngestHarness, words: &BTreeMap<MessageId, String>, at: &str) {
    let conn = h.search_db();
    let want: Vec<MessageId> = words.keys().copied().collect();
    assert_eq!(
        search::indexed(conn).expect("docsize"),
        want,
        "{at}: the index holds the rows of the rule, no gap"
    );
    for (id, word) in words {
        assert_eq!(
            search::ids_with(conn, word).expect("raw match"),
            [*id],
            "{at}: row {} is found by its word",
            id.0
        );
    }
    search::assert_map_holds(conn).expect("the map");
    search::integrity_check(conn).expect("integrity-check");
    assert_eq!(search::queued(h.connection()).expect("queue"), 0, "{at}");
    assert_eq!(meta(conn, "floor"), 0, "{at}: the backfill is over");
}

/// Copies a closed core.db and its search.db, each with the `-wal` it left (the harness's
/// read-only connection to core.db may close last, and a read-only connection leaves the WAL).
fn copy_pair(from: &Path, to: &Path) {
    for (a, b) in [
        (from.to_path_buf(), to.to_path_buf()),
        (search::beside(from), search::beside(to)),
    ] {
        std::fs::copy(&a, &b).expect("copy the file");
        let (mut wa, mut wb) = (a.into_os_string(), b.into_os_string());
        wa.push("-wal");
        wb.push("-wal");
        if Path::new(&wa).exists() {
            std::fs::copy(&wa, &wb).expect("copy its -wal");
        }
    }
}

/// The child of the kill loop: backfills the file in batches of [`BATCH`] until it is done
/// (killed at the n-th batch's commit by `KOURIER_FAILPOINT`).
#[test]
fn backfill_child() {
    let Ok(path) = std::env::var(CHILD_DB) else {
        return;
    };
    let mut h = harness(Path::new(&path));
    h.search_backfill_all(BATCH).expect("backfill");
}

fn run_child(db: &Path, failpoint: Option<&str>) -> ExitStatus {
    let exe = std::env::current_exe().expect("test binary");
    let log = db.with_extension(format!(
        "{}.log",
        if failpoint.is_some() { "kill" } else { "clean" }
    ));
    let mut cmd = Command::new(exe);
    cmd.arg("--exact")
        .arg(support::child_filter(module_path!(), "backfill_child"))
        .args(["--nocapture", "--test-threads=1"])
        .env(CHILD_DB, db)
        .env_remove(kourier_db::failpoint::ENV)
        .stdout(Stdio::null())
        .stderr(Stdio::from(std::fs::File::create(log).expect("log file")));
    if let Some(fp) = failpoint {
        cmd.env(kourier_db::failpoint::ENV, fp);
    }
    let mut child = cmd.spawn().expect("spawn child");
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait().expect("wait") {
            return status;
        }
        if start.elapsed() > CHILD_TIMEOUT {
            let _ = child.kill();
            panic!("child hung (failpoint {failpoint:?})");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// A kill before the commit of every batch in turn; before the restart ONE searchable row of
/// the killed batch is edited (critic H3; one, so the others are not put in the log: the
/// flush would index them again and hide a gap). After the restart the index holds every row
/// of the rule exactly once with its text as it is now, and the old text of the edited row is
/// found nowhere: a cursor saved before its rows leaves a gap, one saved after them a
/// duplicate whose old text stays.
#[test]
fn a_kill_at_every_batch_leaves_no_gap_and_no_duplicate() {
    if std::env::var(CHILD_DB).is_ok() || !kourier_db::failpoint::ENABLED {
        return;
    }
    let base = tempfile::tempdir().expect("tempdir");
    let base_db = base.path().join("core.db");
    let (words, batches) = {
        let mut h = harness(&base_db);
        let words = seed(&mut h, 140);
        let ids = ids_desc(h.connection());
        let batches: Vec<Vec<i64>> = ids.chunks(BATCH).map(<[i64]>::to_vec).collect();
        (words, batches)
    };
    // The harness is closed: the WALs are in the files.
    let mut kills = 0;
    for n in 1.. {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("core.db");
        copy_pair(&base_db, &db);
        let spec = format!("{}:{n}", kourier_core::FAILPOINT_SEARCH_BEFORE_COMMIT);
        let status = run_child(&db, Some(&spec));
        if status.success() {
            assert_eq!(n - 1, batches.len(), "one commit per batch");
            break;
        }
        assert_eq!(status.signal(), Some(SIGABRT), "{spec}: {status:?}");
        kills += 1;
        let mut words = words.clone();
        let mut old = Vec::new();
        {
            let conn = kourier_db::open(&db, &kourier_db::DbKey::from(KEY)).expect("open");
            let id = batches[n - 1]
                .iter()
                .copied()
                .find(|id| words.contains_key(&MessageId(*id)))
                .expect("the batch holds a searchable row");
            let new = format!("redo{id}q");
            conn.execute(
                "UPDATE messages SET body = ?2 WHERE id = ?1",
                (id, text_body(&new)),
            )
            .expect("edit");
            let word = words.get_mut(&MessageId(id)).expect("searchable");
            old.push(std::mem::replace(word, new));
        }
        assert!(run_child(&db, None).success(), "{spec}: the restart");
        let mut h = harness(&db);
        h.search_flush().expect("flush");
        assert_whole(&h, &words, &spec);
        for word in &old {
            assert!(
                search::ids_with(h.search_db(), word)
                    .expect("raw match")
                    .is_empty(),
                "{spec}: the old text of an edited row is still found (a duplicate entry)"
            );
        }
    }
    assert!(kills >= 20, "{kills} kills");
}

/// Rows that change while the backfill runs, above `floor` (indexed already) and below it
/// (waiting), end as they are: an edit's old words gone, a deleted row gone, a row moved to
/// another chat found under that chat's token only; and a change the backfill meets in the
/// log of its own range (below `floor`: the flush leaves it) is read as it is.
#[test]
fn rows_that_change_during_the_backfill_end_as_they_are() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&dir.path().join("core.db"));
    let mut words = seed(&mut h, 60);
    let alice = conv_of(h.connection(), ALICE);
    let carol = conv_of(h.connection(), CAROL);
    assert_eq!(
        search::progress(h.search_db()).expect("progress"),
        (false, 0)
    );
    let one = h.search_backfill_one(BATCH).expect("one batch");
    assert_eq!((one.looked, one.floor), (BATCH, 53));
    assert_eq!(
        search::progress(h.search_db()).expect("progress"),
        (false, 11)
    );
    let id_of = |h: &IngestHarness, n: i64| -> i64 {
        h.connection()
            .query_row(
                "SELECT id FROM messages WHERE net_local = ?1",
                [n.to_string()],
                |r| r.get(0),
            )
            .expect("id")
    };
    let mut gone = Vec::new();
    // 58 and 30 are edited, 56 and 28 deleted, 59 and 31 (Alice's) moved to Carol's chat:
    // the first of each pair is above `floor` (indexed), the second below it (waiting).
    for n in [58, 30] {
        let id = id_of(&h, n);
        let new = format!("edited{n}q");
        h.connection()
            .execute(
                "UPDATE messages SET body = ?2 WHERE id = ?1",
                (id, text_body(&new)),
            )
            .expect("edit");
        gone.push(words.insert(MessageId(id), new).expect("searchable"));
    }
    for n in [56, 28] {
        let id = id_of(&h, n);
        h.connection()
            .execute("DELETE FROM messages WHERE id = ?1", [id])
            .expect("delete");
        gone.push(words.remove(&MessageId(id)).expect("searchable"));
    }
    let moved: Vec<i64> = [59, 31].map(|n| id_of(&h, n)).to_vec();
    for id in &moved {
        h.connection()
            .execute(
                "UPDATE messages SET conversation_id = ?2 WHERE id = ?1",
                (id, carol.0),
            )
            .expect("move");
    }
    // A new row comes in.
    h.apply(vec![BackendEvent::Message(row(61, "newcomerq"))])
        .expect("apply");
    words.insert(MessageId(id_of(&h, 61)), "newcomerq".into());
    // The task's order: the log first.
    h.search_flush().expect("flush");
    // A change after that flush, of a row the backfill has not read: its log row is in the
    // backfill's range, and the backfill reads the row as it is.
    let late = id_of(&h, 20);
    h.connection()
        .execute(
            "UPDATE messages SET body = ?2 WHERE id = ?1",
            (late, text_body("latechangeq")),
        )
        .expect("edit");
    gone.push(
        words
            .insert(MessageId(late), "latechangeq".into())
            .expect("searchable"),
    );
    h.search_backfill_all(BATCH).expect("backfill");
    h.search_flush().expect("flush");
    assert_whole(&h, &words, "after the backfill");
    for word in &gone {
        assert!(
            search::ids_with(h.search_db(), word)
                .expect("raw match")
                .is_empty(),
            "an old word is found"
        );
    }
    for id in moved {
        let word = &words[&MessageId(id)];
        assert_eq!(
            search::ids_with_in(h.search_db(), word, carol).expect("raw match"),
            [MessageId(id)]
        );
        assert!(
            search::ids_with_in(h.search_db(), word, alice)
                .expect("raw match")
                .is_empty(),
            "a moved row is found under its old chat"
        );
    }
    assert_eq!(
        search::progress(h.search_db()).expect("progress"),
        (true, 100)
    );
}

/// History rows carry their sent time, so the rowids (week first) of a batch read newest id
/// first come in any order: one batch is sorted and cuts ONE segment.
#[test]
fn one_backfill_batch_cuts_one_segment() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&dir.path().join("core.db"));
    let week = 7 * 86_400_000;
    let mut events = Vec::new();
    let mut batch = Vec::new();
    for n in 0..300_i64 {
        let weeks_back = (n * 37) % 97;
        let mut m = remote(
            &chat(ALICE),
            &uid(ALICE),
            &format!("s{n}"),
            T0 - weeks_back * week - n,
            &format!("word{n} spreadq"),
        );
        m.read = true;
        batch.push(m);
        if batch.len() == 100 {
            events.push(BackendEvent::HistoryBatch {
                conv: chat(ALICE),
                messages: std::mem::take(&mut batch),
                has_more: true,
                progress: None,
            });
        }
    }
    h.apply(events).expect("apply");
    h.search_rebuild().expect("rebuild");
    let done = h.search_backfill_one(300).expect("one batch");
    assert_eq!((done.looked, done.indexed, done.floor), (300, 300, 0));
    assert_eq!(
        search::ids_with(h.search_db(), "spreadq")
            .expect("raw match")
            .len(),
        300
    );
    assert_eq!(
        search::segments(h.search_db()).expect("segments"),
        1,
        "one batch, one segment"
    );
}

/// A search.db written by other token rules (`norm` names them) is built again at the open,
/// and written again by the backfill: the old form of a word is found nowhere (critic H3's bump
/// oracle; an entry of the old rules beside the new one would stay for good).
#[test]
fn a_new_version_of_the_rules_builds_the_file_again() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&dir.path().join("core.db"));
    let mut words = BTreeMap::new();
    let mut events = Vec::new();
    for n in 1..=20 {
        events.push(BackendEvent::Message(row(n, &format!("rule{n}q"))));
    }
    h.apply(events).expect("apply");
    for id in ids_desc(h.connection()) {
        let n = local_of(h.connection(), id);
        words.insert(MessageId(id), format!("rule{n}q"));
    }
    h.search_flush().expect("flush");
    // The rules of this build change nothing: the file belongs.
    assert_eq!(h.search_open().expect("open"), None);
    assert_eq!(search::indexed(h.search_db()).expect("docsize").len(), 20);
    // As an older build wrote it: row 5's word had another token, and `norm` names the old
    // rules.
    let (id, sort_key, conv): (i64, i64, i64) = h
        .connection()
        .query_row(
            "SELECT id, sort_key, conversation_id FROM messages WHERE net_local = '5'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .expect("row 5");
    let rowid = search::rowid_of(sort_key, MessageId(id)).expect("rowid");
    let s = h.search_db();
    s.execute("DELETE FROM messages_fts WHERE rowid = ?1", [rowid])
        .expect("old entry out");
    s.execute(
        "INSERT INTO messages_fts(rowid, words, meta) VALUES (?1, 'oldruleformq common', ?2)",
        (rowid, format!("c{conv}")),
    )
    .expect("old entry in");
    search::set_meta(s, "norm", i64::from(search::NORMALIZER_VERSION) - 1).expect("old rules");
    assert_eq!(
        search::ids_with(h.search_db(), "oldruleformq").expect("raw match"),
        [MessageId(id)],
        "the old form is in the index before the bump"
    );
    // A row changed before the bump leaves a row in the log.
    let seven: i64 = h
        .connection()
        .query_row("SELECT id FROM messages WHERE net_local = '7'", [], |r| {
            r.get(0)
        })
        .expect("row 7");
    h.connection()
        .execute(
            "UPDATE messages SET body = ?2 WHERE id = ?1",
            (seven, text_body("rule7newq")),
        )
        .expect("edit");
    words.insert(MessageId(seven), "rule7newq".into());
    assert_eq!(search::queued(h.connection()).expect("queue"), 1);
    assert_eq!(
        h.search_open().expect("open"),
        Some(search::Rebuild::Norm),
        "the file is built again"
    );
    assert_eq!(
        meta(h.search_db(), "floor"),
        ids_desc(h.connection())[0],
        "every row waits again"
    );
    assert!(
        search::indexed(h.search_db()).expect("docsize").is_empty(),
        "the index is empty"
    );
    assert_eq!(
        meta(h.search_db(), "norm"),
        i64::from(search::NORMALIZER_VERSION)
    );
    h.search_backfill_all(BATCH).expect("backfill");
    h.search_flush().expect("flush");
    assert_whole(&h, &words, "after the bump");
    assert!(
        search::ids_with(h.search_db(), "oldruleformq")
            .expect("raw match")
            .is_empty(),
        "the old form is found after the bump"
    );
    assert!(
        search::ids_with(h.search_db(), "rule7q")
            .expect("raw match")
            .is_empty()
    );
}

/// [`Env::restart`] with the backfill's delay of the test's choosing.
async fn restart_with(env: &mut Env, backfill: Duration) {
    env.stop().await;
    let networks = NetworkRegistry::new()
        .with(Arc::new(env.fake.clone()))
        .expect("registry");
    let timeouts = Timeouts {
        shutdown_grace: Duration::from_secs(5),
        logout: Duration::from_secs(5),
        search_backfill: backfill,
        ..Timeouts::default()
    };
    let core = support::start_core_timed(
        env.dir.path(),
        networks,
        &env.vault,
        Arc::new(env.clock.clone()),
        timeouts,
    )
    .await;
    env.handle = core.handle();
    env.events = env.handle.subscribe();
    env.core = Some(core);
}

/// A writable connection to the running core's search.db (to damage it).
fn writable_search(env: &Env) -> kourier_db::rusqlite::Connection {
    kourier_db::open(&env.search_db(), &kourier_db::DbKey::from(SEARCH_KEY)).expect("open")
}

/// Whether the running core's search.db is there and its backfill is over.
fn filled(env: &Env) -> bool {
    // The file can be there a moment before its schema: that is not filled.
    kourier_db::open_read_only(&env.search_db(), &kourier_db::DbKey::from(SEARCH_KEY))
        .ok()
        .and_then(|c| search::meta(&c, "floor").ok())
        == Some(0)
}

/// Through the running core: a core.db whose rows were there before search.db (the first
/// start of this build) gets its search.db only at `UiReady`, with every row below `floor`,
/// and after the backfill's delay all of them are written; a damaged index takes the
/// rebuild's path (decisions M9) and is whole again, with what was written meanwhile.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_running_core_backfills_after_ui_ready_and_builds_a_damaged_index_again() {
    let deliver: Vec<BackendEvent> = (1..=30)
        .map(|n| BackendEvent::Message(row(n, &format!("start{n}q"))))
        .collect();
    let script = Script::new("search", vec![uid(ME)]).deliver(deliver);
    let mut env = Env::new(Flavor::SignalLike, script).await;
    let account = add_linked(&mut env).await;
    env.wait_finished(account).await;
    // As at the first start of this build: core.db holds rows and no search.db stands.
    env.stop().await;
    for side in ["", "-wal", "-shm"] {
        let mut name = env.search_db().into_os_string();
        name.push(side);
        let _ = std::fs::remove_file(name);
    }
    restart_with(&mut env, Duration::from_millis(300)).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(
        !env.search_db().exists(),
        "nothing is made or read before UiReady"
    );
    env.handle.send(UiCommand::UiReady).expect("send");
    let words: Vec<String> = (1..=30).map(|n| format!("start{n}q")).collect();
    let all_found = |env: &Env, extra: Option<&str>| {
        filled(env)
            && env.peek_search(|c| {
                words.iter().map(String::as_str).chain(extra).all(|w| {
                    search::ids_with(c, w)
                        .map(|ids| ids.len() == 1)
                        .unwrap_or(false)
                })
            })
    };
    eventually("the backfill writes every row", WAIT, || {
        let done = all_found(&env, None);
        async move { done }
    })
    .await;
    let top = env.peek_search(|c| meta(c, "top"));
    assert!(
        top >= 30,
        "the file was made with every row below its floor"
    );
    // The index is damaged (its structure record is gone): the next write fails, and the
    // task builds the file again.
    writable_search(&env)
        .execute("DELETE FROM messages_fts_data WHERE id = 10", [])
        .expect("damage");
    let conv: ConversationId = env.peek(|c| {
        c.query_row("SELECT id FROM conversations LIMIT 1", [], |r| {
            r.get(0).map(ConversationId)
        })
        .expect("chat")
    });
    env.handle
        .send(UiCommand::SendText {
            conv,
            text: Text::plain("aftermendq"),
            reply_to: None,
        })
        .expect("send");
    eventually("the damaged index is whole again", WAIT, || {
        let done = all_found(&env, Some("aftermendq"));
        async move { done }
    })
    .await;
    search::integrity_check(&writable_search(&env)).expect("integrity-check");
    search::assert_map_holds(&writable_search(&env)).expect("the map");
    env.stop().await;
}
