//! Kills at every window of search.db's writes (the fix round, design section 7): before the
//! COMMIT of each flush job, between that commit and the core's prune, and inside each step of
//! a compaction. After each kill and a restart every message is found exactly once by the
//! words it has NOW, an edited row's old words and a deleted row's words never, the `indexed`
//! map names exactly the entries, and FTS5's own check passes. The backfill batch's loop is
//! `search_backfill::a_kill_at_every_batch_leaves_no_gap_and_no_duplicate` (its seed and its
//! oracle live there). The texts are synthetic, each word one no other row holds.

use crate::support;

use std::collections::BTreeMap;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use kourier_backend_api::{BackendEvent, IncomingMessage};
use kourier_core::testing::{IngestHarness, search};
use kourier_fake::scripts::{T0, remote};
use kourier_model::{ConvKey, MessageId, MsgIdScope, NetworkKind, UserId};

const ME: &str = "aci:00000000-0000-4000-8000-0000000000aa";
const ALICE: &str = "aci:00000000-0000-4000-8000-0000000a11ce";

const KEY: [u8; 32] = [8; 32];
const CHILD_DB: &str = "KOURIER_TEST_SEARCH_KILL_WRITES_DB";
const CHILD_MODE: &str = "KOURIER_TEST_SEARCH_KILL_WRITES_MODE";
const SIGABRT: i32 = 6;
const CHILD_TIMEOUT: Duration = Duration::from_secs(120);
/// A flush job's batch: the 27 log rows of [`pending`] give 7 jobs.
const BATCH: usize = 4;
const NOW: i64 = T0 + 86_400_000;

fn uid(s: &str) -> UserId {
    UserId::parse(s).expect("user id")
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

fn row(n: i64, text: &str) -> IncomingMessage {
    let alice = uid(ALICE);
    remote(
        &ConvKey::Direct(alice.clone()),
        &alice,
        &n.to_string(),
        T0 + n * 1000,
        text,
    )
}

fn id_of(h: &IngestHarness, n: i64) -> MessageId {
    h.connection()
        .query_row(
            "SELECT id FROM messages WHERE net_local = ?1",
            [n.to_string()],
            |r| r.get(0).map(MessageId),
        )
        .expect("row")
}

fn body(word: &str) -> String {
    format!("{{\"Text\":{{\"body\":\"{word}\"}}}}")
}

/// What the index must hold: the word of every live row, and the words that must be found
/// nowhere.
#[derive(Clone, Default)]
struct Oracle {
    live: BTreeMap<MessageId, String>,
    never: Vec<String>,
}

/// Rows `1..=n` with the word `w<n>q`, flushed in jobs of `job` (one segment each).
fn written(h: &mut IngestHarness, n: i64, job: usize) -> Oracle {
    let events = (1..=n)
        .map(|i| BackendEvent::Message(row(i, &format!("w{i}q"))))
        .collect();
    h.apply(events).expect("apply");
    loop {
        if h.search_flush_one(job).expect("flush").taken == 0 {
            break;
        }
    }
    let mut oracle = Oracle::default();
    for i in 1..=n {
        oracle.live.insert(id_of(h, i), format!("w{i}q"));
    }
    oracle
}

/// The changes the flush loop's kills fall between (27 log rows, NOT flushed): rows 1-10
/// edited, 11-15 deleted, 16 moved to another hour and edited, 41-50 new.
fn pending(h: &mut IngestHarness, mut oracle: Oracle) -> Oracle {
    for i in 1..=10 {
        let id = id_of(h, i);
        h.connection()
            .execute(
                "UPDATE messages SET body = ?2 WHERE id = ?1",
                (id.0, body(&format!("e{i}q"))),
            )
            .expect("edit");
        oracle.never.push(format!("w{i}q"));
        oracle.live.insert(id, format!("e{i}q"));
    }
    for i in 11..=15 {
        let id = id_of(h, i);
        h.connection()
            .execute("DELETE FROM messages WHERE id = ?1", [id.0])
            .expect("delete");
        oracle.never.push(format!("w{i}q"));
        oracle.live.remove(&id);
    }
    let sixteen = id_of(h, 16);
    h.connection()
        .execute(
            "UPDATE messages SET sort_key = sort_key + 7200000 * 1024 WHERE id = ?1",
            [sixteen.0],
        )
        .expect("move");
    h.connection()
        .execute(
            "UPDATE messages SET body = ?2 WHERE id = ?1",
            (sixteen.0, body("m16q")),
        )
        .expect("edit");
    oracle.never.push("w16q".into());
    oracle.live.insert(sixteen, "m16q".into());
    let events = (41..=50)
        .map(|i| BackendEvent::Message(row(i, &format!("n{i}q"))))
        .collect();
    h.apply(events).expect("apply");
    for i in 41..=50 {
        oracle.live.insert(id_of(h, i), format!("n{i}q"));
    }
    assert_eq!(search::queued(h.connection()).expect("queue"), 27);
    oracle
}

/// The index holds exactly the oracle.
fn assert_whole(h: &IngestHarness, oracle: &Oracle, at: &str) {
    let conn = h.search_db();
    assert_eq!(
        search::indexed(conn).expect("docsize"),
        oracle.live.keys().copied().collect::<Vec<_>>(),
        "{at}: one entry per live row"
    );
    for (id, word) in &oracle.live {
        assert_eq!(
            search::ids_with(conn, word).expect("match"),
            [*id],
            "{at}: row {} is found by its word, once",
            id.0
        );
    }
    for word in &oracle.never {
        assert!(
            search::ids_with(conn, word).expect("match").is_empty(),
            "{at}: an old or deleted word is found"
        );
    }
    search::assert_map_holds(conn).expect("the map");
    search::integrity_check(conn).expect("integrity-check");
}

/// Copies a closed core.db and its search.db (each with its `-wal`, if one was left).
fn copy_pair(from: &Path, to: &Path) {
    for (a, b) in [
        (from.to_owned(), to.to_owned()),
        (search::beside(from), search::beside(to)),
    ] {
        for side in ["", "-wal"] {
            let mut src = a.as_os_str().to_owned();
            src.push(side);
            if Path::new(&src).exists() {
                let mut dst = b.as_os_str().to_owned();
                dst.push(side);
                std::fs::copy(&src, &dst).expect("copy");
            }
        }
    }
}

/// The child of the loops: `flush` runs flush jobs of [`BATCH`] until the log is empty;
/// `compact` runs the asked compaction to its end.
#[test]
fn kill_writes_child() {
    let (Ok(path), Ok(mode)) = (std::env::var(CHILD_DB), std::env::var(CHILD_MODE)) else {
        return;
    };
    let mut h = harness(Path::new(&path));
    match mode.as_str() {
        "flush" => loop {
            if h.search_flush_one(BATCH).expect("flush").taken == 0 {
                break;
            }
        },
        "compact" => {
            let asked = search::compact_after(h.connection()).expect("ask");
            search::compact_all(h.search_writer(), asked, NOW).expect("compaction");
        }
        other => panic!("mode {other}"),
    }
}

fn run_child(db: &Path, mode: &str, failpoint: Option<&str>) -> ExitStatus {
    let exe = std::env::current_exe().expect("test binary");
    let log = db.with_extension(format!(
        "{mode}.{}.log",
        if failpoint.is_some() { "kill" } else { "clean" }
    ));
    let mut cmd = Command::new(exe);
    cmd.arg("--exact")
        .arg(support::child_filter(module_path!(), "kill_writes_child"))
        .args(["--nocapture", "--test-threads=1"])
        .env(CHILD_DB, db)
        .env(CHILD_MODE, mode)
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

/// Runs the child under `point:n` for n = 1, 2, … until it ends by itself; after every kill
/// `check` gets the killed copy. Returns the number of kills.
fn every_hit(base: &Path, mode: &str, point: &str, mut check: impl FnMut(&Path, &str)) -> usize {
    let mut kills = 0;
    for n in 1.. {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("core.db");
        copy_pair(base, &db);
        let spec = format!("{point}:{n}");
        let status = run_child(&db, mode, Some(&spec));
        if status.success() {
            break;
        }
        assert_eq!(status.signal(), Some(SIGABRT), "{spec}: {status:?}");
        kills += 1;
        check(&db, &spec);
        assert!(n < 1_000, "the child ends");
    }
    kills
}

/// A kill before search.db's commit of every flush job, and one between every commit and the
/// core's prune: the restart flushes what is left and the index is whole (a cursor committed
/// before its writes leaves a gap; a prune above `applied` loses a change; a missing take-out
/// leaves an old word).
#[test]
fn a_kill_around_every_flush_commit_leaves_the_index_whole() {
    if std::env::var(CHILD_DB).is_ok() || !kourier_db::failpoint::ENABLED {
        return;
    }
    let base = tempfile::tempdir().expect("tempdir");
    let base_db = base.path().join("core.db");
    let oracle = {
        let mut h = harness(&base_db);
        let oracle = written(&mut h, 40, 10);
        pending(&mut h, oracle)
    };
    for point in [
        kourier_core::FAILPOINT_SEARCH_BEFORE_COMMIT,
        kourier_core::FAILPOINT_SEARCH_AFTER_COMMIT,
    ] {
        let kills = every_hit(&base_db, "flush", point, |db, spec| {
            let mut h = harness(db);
            assert_eq!(h.search_judged(), None, "{spec}: the file belongs");
            h.search_flush().expect("flush");
            assert_eq!(search::queued(h.connection()).expect("queue"), 0, "{spec}");
            assert_whole(&h, &oracle, spec);
        });
        assert_eq!(kills, 7, "{point}: one kill per flush job");
    }
}

/// A kill inside every step of a compaction: the index is whole after the restart (a merge
/// is one transaction), and the compaction run again ends with no tombstone left.
#[test]
fn a_kill_in_every_compaction_step_leaves_the_index_whole() {
    if std::env::var(CHILD_DB).is_ok() || !kourier_db::failpoint::ENABLED {
        return;
    }
    let base = tempfile::tempdir().expect("tempdir");
    let base_db = base.path().join("core.db");
    let oracle = {
        let mut h = harness(&base_db);
        let mut oracle = written(&mut h, 600, 25);
        // One row in fifteen, spread over every level: FTS5 folds a level whose rows are 10 %
        // or more deleted by itself in the flush that wrote the tombstones (its `deletemerge`),
        // and the compaction would then find nothing to do.
        let gone: Vec<MessageId> = oracle.live.keys().copied().step_by(15).collect();
        for id in gone {
            h.connection()
                .execute("DELETE FROM messages WHERE id = ?1", [id.0])
                .expect("delete");
            oracle.never.push(oracle.live.remove(&id).expect("live"));
        }
        search::ask_now(h.connection()).expect("ask");
        h.search_flush().expect("flush");
        assert_eq!(search::meta(h.search_db(), "dead").expect("dead"), 40);
        oracle
    };
    let kills = every_hit(
        &base_db,
        "compact",
        kourier_core::FAILPOINT_SEARCH_COMPACT_STEP,
        |db, spec| {
            let mut h = harness(db);
            assert_eq!(h.search_judged(), None, "{spec}: the file belongs");
            assert_whole(&h, &oracle, spec);
            let asked = search::compact_after(h.connection()).expect("ask");
            search::compact_all(h.search_writer(), asked, NOW).expect("compaction");
            assert_eq!(
                search::meta(h.search_db(), "dead").expect("dead"),
                0,
                "{spec}: the compaction run again takes every tombstone"
            );
            assert_whole(&h, &oracle, spec);
        },
    );
    assert!(
        kills >= 2,
        "{kills} kills: the compaction takes several steps"
    );
}
