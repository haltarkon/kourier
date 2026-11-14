//! Kills while search.db is made or built again (the fix round, design section 7): after the
//! old file and its side files are deleted and before the new one is made, and before the
//! commit of a new file's schema and numbers (a first file, and the file of a rebuild). The
//! restart finds no file, or one without its schema, makes it, and after the backfill and a
//! flush the index finds every row of core.db exactly once and nothing of what the old file
//! held of rows core.db no longer has. The texts are synthetic, each word one no other row
//! holds.

use crate::support;

use std::collections::BTreeMap;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use kourier_backend_api::BackendEvent;
use kourier_core::testing::{IngestHarness, search};
use kourier_fake::scripts::{T0, remote};
use kourier_model::{ConvKey, MessageId, MsgIdScope, NetworkKind, UserId};

const ME: &str = "aci:00000000-0000-4000-8000-0000000000aa";
const ALICE: &str = "aci:00000000-0000-4000-8000-0000000a11ce";

const KEY: [u8; 32] = [10; 32];
const CHILD_DB: &str = "KOURIER_TEST_SEARCH_KILL_FILE_DB";
const SIGABRT: i32 = 6;
const CHILD_TIMEOUT: Duration = Duration::from_secs(120);

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

fn id_of(h: &IngestHarness, n: i64) -> MessageId {
    h.connection()
        .query_row(
            "SELECT id FROM messages WHERE net_local = ?1",
            [n.to_string()],
            |r| r.get(0).map(MessageId),
        )
        .expect("row")
}

/// Removes search.db and its side files.
fn remove_search(core: &Path) {
    let path = search::beside(core);
    let name = path
        .file_name()
        .expect("name")
        .to_string_lossy()
        .into_owned();
    for side in ["-wal", "-shm", "-journal", ""] {
        let _ = std::fs::remove_file(path.with_file_name(format!("{name}{side}")));
    }
}

/// A base pair: rows 1-20 indexed, then rows 5-8 deleted in core.db and NOT flushed (the old
/// file still holds their words). `fresh`: search.db is removed (the first start of this
/// build); else it is marked another version (it does not belong and is built again).
/// Returns the words of the rows core.db holds, and the deleted rows' words.
fn base(core: &Path, fresh: bool) -> (BTreeMap<MessageId, String>, Vec<String>) {
    let mut words = BTreeMap::new();
    let mut gone = Vec::new();
    {
        let mut h = harness(core);
        let alice = uid(ALICE);
        let events = (1..=20)
            .map(|n| {
                BackendEvent::Message(remote(
                    &ConvKey::Direct(alice.clone()),
                    &alice,
                    &n.to_string(),
                    T0 + n * 1000,
                    &format!("file{n}q"),
                ))
            })
            .collect();
        h.apply(events).expect("apply");
        h.search_flush().expect("flush");
        for n in 1..=20 {
            let id = id_of(&h, n);
            if (5..=8).contains(&n) {
                h.connection()
                    .execute("DELETE FROM messages WHERE id = ?1", [id.0])
                    .expect("delete");
                gone.push(format!("file{n}q"));
            } else {
                words.insert(id, format!("file{n}q"));
            }
        }
        if !fresh {
            search::set_meta(h.search_db(), "version", search::SEARCH_FILE_VERSION + 1)
                .expect("another version");
        }
    }
    if fresh {
        remove_search(core);
    }
    (words, gone)
}

/// The child: opens the harness, which makes or builds search.db again.
#[test]
fn kill_file_child() {
    let Ok(path) = std::env::var(CHILD_DB) else {
        return;
    };
    let _h = harness(Path::new(&path));
}

fn run_child(db: &Path, failpoint: Option<&str>) -> ExitStatus {
    let exe = std::env::current_exe().expect("test binary");
    let mut cmd = Command::new(exe);
    cmd.arg("--exact")
        .arg(support::child_filter(module_path!(), "kill_file_child"))
        .args(["--nocapture", "--test-threads=1"])
        .env(CHILD_DB, db)
        .env_remove(kourier_db::failpoint::ENV)
        .stdout(Stdio::null())
        .stderr(Stdio::from(
            std::fs::File::create(db.with_extension("child.log")).expect("log file"),
        ));
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

/// Every kill point of making the file, for a first file and for a rebuild: the child dies at
/// the point, the restart judges what it left (`judged`) and the index ends whole.
#[test]
fn a_kill_while_the_file_is_made_or_built_again_leaves_nothing_wrong() {
    if std::env::var(CHILD_DB).is_ok() || !kourier_db::failpoint::ENABLED {
        return;
    }
    let cases = [
        // A first file: the kill before its commit leaves a file without its schema.
        (
            true,
            kourier_core::FAILPOINT_SEARCH_CREATE_BEFORE_COMMIT,
            Some(search::Rebuild::Unfinished),
        ),
        // A rebuild: the old file is gone, the new one not made yet.
        (false, kourier_core::FAILPOINT_SEARCH_REBUILD_DELETED, None),
        // A rebuild: the main file is the first to go, the old file's side files are not
        // touched yet (review P3): the restart finds no main file at all and makes a clean
        // one without judging anything. With the main file removed LAST the old file would
        // still be there, and judged again.
        (false, kourier_core::FAILPOINT_SEARCH_REMOVE_PARTWAY, None),
        // A rebuild: the new file without its schema.
        (
            false,
            kourier_core::FAILPOINT_SEARCH_CREATE_BEFORE_COMMIT,
            Some(search::Rebuild::Unfinished),
        ),
    ];
    for (fresh, point, judged) in cases {
        let dir = tempfile::tempdir().expect("tempdir");
        let core = dir.path().join("core.db");
        let (words, gone) = base(&core, fresh);
        let spec = format!("{point}:1");
        let status = run_child(&core, Some(&spec));
        assert_eq!(
            status.signal(),
            Some(SIGABRT),
            "{spec} (fresh {fresh}): {status:?}"
        );
        if point == kourier_core::FAILPOINT_SEARCH_REBUILD_DELETED
            || point == kourier_core::FAILPOINT_SEARCH_REMOVE_PARTWAY
        {
            assert!(
                !search::beside(&core).exists(),
                "{spec}: the old file is gone before the new one is made"
            );
        }
        let mut h = harness(&core);
        assert_eq!(h.search_judged(), judged, "{spec} (fresh {fresh})");
        h.search_backfill_all(5).expect("backfill");
        h.search_flush().expect("flush");
        let conn = h.search_db();
        assert_eq!(
            search::indexed(conn).expect("docsize"),
            words.keys().copied().collect::<Vec<_>>(),
            "{spec}: every row of core.db, no other"
        );
        for (id, word) in &words {
            assert_eq!(
                search::ids_with(conn, word).expect("match"),
                [*id],
                "{spec}"
            );
        }
        for word in &gone {
            assert!(
                search::ids_with(conn, word).expect("match").is_empty(),
                "{spec}: a deleted row's word"
            );
        }
        search::assert_map_holds(conn).expect("the map");
        search::integrity_check(conn).expect("integrity-check");
        drop(h);
        // Nothing more to judge: the next open keeps the file.
        assert_eq!(harness(&core).search_judged(), None, "{spec}");
    }
}
