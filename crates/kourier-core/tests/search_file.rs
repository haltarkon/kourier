//! The open checks of search.db (the fix round, design section 4; section 7 asks one test
//! each): a file that cannot be decrypted (another key, garbage), that is damaged, that was left
//! without its schema, of another `SEARCH_FILE_VERSION`, made for another core.db (its epoch),
//! or whose `applied` is ahead of core.db's log (an older copy of core.db under the same epoch)
//! is BUILT AGAIN, and the rebuilt index, after its backfill and a flush, finds exactly what
//! core.db holds. Another `NORMALIZER_VERSION` is `search_backfill::
//! a_new_version_of_the_rules_builds_the_file_again`. Every look is at the index itself
//! (`testing::search`); the texts are synthetic, each word one no other row holds.

use std::collections::BTreeMap;
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;

use std::sync::Arc;

use kourier_backend_api::{BackendEvent, IncomingMessage, NetworkRegistry, SystemClock};
use kourier_core::testing::{DirVault, IngestHarness, search};
use kourier_core::{Core, CoreConfig, CoreError};
use kourier_fake::scripts::{T0, remote};
use kourier_model::{ConvKey, MessageId, MsgIdScope, NetworkKind, UserId};

const ME: &str = "aci:00000000-0000-4000-8000-0000000000aa";
const ALICE: &str = "aci:00000000-0000-4000-8000-0000000a11ce";

const KEY: [u8; 32] = [7; 32];
const BATCH: usize = 5;

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

/// Rows `from..=to` with the word `<stem><n>q` each, flushed into the index; returns the word
/// of each row by its id.
fn write_rows(
    h: &mut IngestHarness,
    stem: &str,
    from: i64,
    to: i64,
) -> BTreeMap<MessageId, String> {
    let events = (from..=to)
        .map(|n| BackendEvent::Message(row(n, &format!("{stem}{n}q"))))
        .collect();
    h.apply(events).expect("apply");
    let mut words = BTreeMap::new();
    for n in from..=to {
        let id: i64 = h
            .connection()
            .query_row(
                "SELECT id FROM messages WHERE net_local = ?1",
                [n.to_string()],
                |r| r.get(0),
            )
            .expect("row");
        words.insert(MessageId(id), format!("{stem}{n}q"));
    }
    h.search_flush().expect("flush");
    words
}

fn meta(h: &IngestHarness, key: &str) -> i64 {
    search::meta(h.search_db(), key).expect("meta")
}

fn max_id(h: &IngestHarness) -> i64 {
    h.connection()
        .query_row("SELECT coalesce(max(id), 0) FROM messages", [], |r| {
            r.get(0)
        })
        .expect("max id")
}

/// A file just built again: empty but for rows written since (through the log, above
/// `floor`), every row of core.db waiting below `floor`, the numbers of this build; then the
/// backfill and a flush, and the index holds exactly `words` (each found by its word), nothing
/// of `gone`, and FTS5's own check passes.
fn rebuilt_holds(h: &mut IngestHarness, words: &BTreeMap<MessageId, String>, gone: &[&str]) {
    // Every row core.db held at the rebuild waits below `floor` (a row written since went
    // through the log).
    let top = meta(h, "top");
    let floor = meta(h, "floor");
    assert!(
        search::indexed(h.search_db())
            .expect("docsize")
            .iter()
            .all(|id| id.0 > floor),
        "a rebuilt file starts empty"
    );
    assert_eq!(meta(h, "floor"), top, "every row waits for the backfill");
    assert!(
        top > 0 && top <= max_id(h),
        "the file was made with core.db's rows"
    );
    assert_eq!(meta(h, "version"), search::SEARCH_FILE_VERSION);
    assert_eq!(meta(h, "norm"), i64::from(search::NORMALIZER_VERSION));
    assert_eq!(meta(h, "dead"), 0);
    assert_eq!(
        meta(h, "applied"),
        search::position(h.connection()).expect("position"),
        "the new file starts at core.db's log"
    );
    h.search_backfill_all(BATCH).expect("backfill");
    h.search_flush().expect("flush");
    let conn = h.search_db();
    assert_eq!(
        search::indexed(conn).expect("docsize"),
        words.keys().copied().collect::<Vec<_>>(),
        "the rebuilt index holds every row, no other"
    );
    for (id, word) in words {
        assert_eq!(
            search::ids_with(conn, word).expect("match"),
            [*id],
            "{word}"
        );
    }
    for word in gone {
        assert!(
            search::ids_with(conn, word).expect("match").is_empty(),
            "{word} is still found"
        );
    }
    assert_eq!(meta(h, "floor"), 0, "the backfill is over");
    assert_eq!(meta(h, "dead"), 0, "a backfill leaves no tombstone");
    search::assert_map_holds(conn).expect("the map");
    search::integrity_check(conn).expect("integrity-check");
}

/// Deletes search.db and what SQLite keeps beside it.
fn remove_search(path: &Path) {
    let name = path
        .file_name()
        .expect("name")
        .to_string_lossy()
        .into_owned();
    for side in ["-wal", "-shm", "-journal", ""] {
        let _ = std::fs::remove_file(path.with_file_name(format!("{name}{side}")));
    }
}

/// A file that belongs is kept as it is: no rebuild, the index and its numbers stay.
#[test]
fn a_file_that_belongs_is_kept() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&dir.path().join("core.db"));
    let words = write_rows(&mut h, "keep", 1, 12);
    let applied = meta(&h, "applied");
    assert_eq!(h.search_open().expect("open"), None);
    assert_eq!(meta(&h, "applied"), applied);
    assert_eq!(meta(&h, "floor"), 0);
    assert_eq!(
        search::indexed(h.search_db()).expect("docsize"),
        words.keys().copied().collect::<Vec<_>>()
    );
}

/// Garbage in the file's place, and a file of another key: neither decrypts, both are built
/// again.
#[test]
fn a_file_of_another_key_or_garbage_is_built_again() {
    for case in ["garbage", "another key"] {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut h = harness(&dir.path().join("core.db"));
        let words = write_rows(&mut h, "key", 1, 12);
        let why = h
            .search_open_after(|path| {
                remove_search(path);
                if case == "garbage" {
                    let junk: Vec<u8> = (0..8192_u32)
                        .map(|i| u8::try_from((i * 37 + 11) % 251).expect("byte"))
                        .collect();
                    std::fs::write(path, junk).expect("garbage");
                } else {
                    let other = kourier_db::open(path, &kourier_db::DbKey::from([9; 32]))
                        .expect("another key");
                    other
                        .execute_batch("CREATE TABLE meta(key TEXT PRIMARY KEY, value INTEGER)")
                        .expect("a table");
                }
            })
            .expect("open");
        assert_eq!(why, Some(search::Rebuild::Unreadable), "{case}");
        rebuilt_holds(&mut h, &words, &[]);
    }
}

/// A page of the file is overwritten (SQLCipher's page check fails): the file is damaged and
/// built again.
#[test]
fn a_damaged_file_is_built_again() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&dir.path().join("core.db"));
    let words = write_rows(&mut h, "harm", 1, 12);
    let why = h
        .search_open_after(|path| {
            let mut name = path.as_os_str().to_owned();
            name.push("-wal");
            assert!(
                !Path::new(&name).exists(),
                "the closed writer left its pages in the file"
            );
            // Page 2 is `meta`, the first table of the schema.
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .open(path)
                .expect("file");
            f.seek(SeekFrom::Start(4096 + 512)).expect("seek");
            f.write_all(&[0x5a; 64]).expect("damage");
        })
        .expect("open");
    assert_eq!(why, Some(search::Rebuild::Damaged));
    rebuilt_holds(&mut h, &words, &[]);
}

/// A kill while the file was made leaves it without its schema: built again.
#[test]
fn a_file_without_its_schema_is_built_again() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&dir.path().join("core.db"));
    let words = write_rows(&mut h, "bare", 1, 12);
    let why = h
        .search_open_after(|path| {
            remove_search(path);
            let bare = kourier_db::open(path, &kourier_db::DbKey::from(KEY)).expect("bare file");
            bare.execute_batch("CREATE TABLE x(a); DROP TABLE x;")
                .expect("a header");
        })
        .expect("open");
    assert_eq!(why, Some(search::Rebuild::Unfinished));
    rebuilt_holds(&mut h, &words, &[]);
}

/// Another version of the file's schema: built again.
#[test]
fn a_file_of_another_version_is_built_again() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&dir.path().join("core.db"));
    let words = write_rows(&mut h, "ver", 1, 12);
    search::set_meta(h.search_db(), "version", search::SEARCH_FILE_VERSION + 1)
        .expect("another version");
    assert_eq!(
        h.search_open().expect("open"),
        Some(search::Rebuild::Version)
    );
    rebuilt_holds(&mut h, &words, &[]);
}

/// A file made for another core.db (a restore, a replaced core.db: another epoch) is built
/// again; what its index held of rows that core.db does not have is gone.
#[test]
fn a_file_of_another_core_db_is_built_again() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&dir.path().join("core.db"));
    let mut words = write_rows(&mut h, "epo", 1, 12);
    // An entry core.db knows nothing of, as a file of another core.db holds.
    let stray = search::rowid_of(T0 * 1024, MessageId(9_999)).expect("rowid");
    h.search_db()
        .execute(
            "INSERT INTO messages_fts(rowid, words, meta) VALUES (?1, 'strayepochq', 'c1')",
            [stray],
        )
        .expect("a stray entry");
    search::new_epoch(h.connection()).expect("another epoch");
    assert_eq!(h.search_open().expect("open"), Some(search::Rebuild::Epoch));
    // A row after the rebuild goes through the log.
    words.extend(write_rows(&mut h, "epo", 13, 14));
    rebuilt_holds(&mut h, &words, &["strayepochq"]);
}

/// Copies core.db and its `-wal` (the harness is closed: its read-only connection may have
/// left the WAL).
fn save_core(core: &Path, to: &Path) {
    std::fs::create_dir_all(to).expect("dir");
    for side in ["", "-wal"] {
        let mut from = core.as_os_str().to_owned();
        from.push(side);
        let from = Path::new(&from);
        if from.exists() {
            std::fs::copy(from, to.join(format!("core.db{side}"))).expect("copy");
        }
    }
}

/// Puts the copy back: core.db's `-wal` and `-shm` go first.
fn restore_core(from: &Path, core: &Path) {
    for side in ["-wal", "-shm"] {
        let mut name = core.as_os_str().to_owned();
        name.push(side);
        let _ = std::fs::remove_file(name);
    }
    for side in ["", "-wal"] {
        let saved = from.join(format!("core.db{side}"));
        if saved.exists() {
            let mut to = core.as_os_str().to_owned();
            to.push(side);
            std::fs::copy(saved, to).expect("restore");
        }
    }
}

/// An OLDER COPY of core.db under the same epoch (a backup put back, search.db kept): its log
/// is behind what search.db applied, so its next seqs would be skipped and its next ids would
/// meet the entries of rows the copy never had. The file is built again: the copy's rows are
/// found, the newer rows' words are not, and a row written after the restore (its id and its
/// seq those of a newer row) is found by its own word only.
#[test]
fn an_older_copy_of_core_db_builds_the_file_again() {
    let dir = tempfile::tempdir().expect("tempdir");
    let core = dir.path().join("core.db");
    let saved = dir.path().join("saved");
    let mut words = {
        let mut h = harness(&core);
        write_rows(&mut h, "old", 1, 10)
    };
    save_core(&core, &saved);
    {
        let mut h = harness(&core);
        assert_eq!(h.search_judged(), None, "the file belongs");
        let newer = write_rows(&mut h, "newer", 11, 20);
        assert_eq!(newer.len(), 10);
        assert!(meta(&h, "applied") > 10);
    }
    restore_core(&saved, &core);
    let mut h = harness(&core);
    assert_eq!(h.search_judged(), Some(search::Rebuild::LogBehind));
    let gone: Vec<String> = (11..=20).map(|n| format!("newer{n}q")).collect();
    let gone: Vec<&str> = gone.iter().map(String::as_str).collect();
    // A row after the restore takes the id and the seq of a newer row.
    let last_old = words.keys().last().expect("ids").0;
    let after = write_rows(&mut h, "after", 21, 21);
    assert!(
        after
            .keys()
            .all(|id| id.0 > last_old && id.0 <= last_old + 10),
        "the id of a newer row is taken again"
    );
    words.extend(after);
    rebuilt_holds(&mut h, &words, &gone);
}

/// AN OLDER search.db beside a core.db whose log was pruned past its `applied` (a copy of the
/// file put back, a mixed-age restore; review D1): the log no longer holds the rows the file
/// never applied, so its next flush would skip a delete and the new rows for good. The file is
/// built again with `LogPruned`: the deleted row's words are gone, the new rows are found.
#[test]
fn an_older_search_db_beside_a_pruned_log_builds_the_file_again() {
    let dir = tempfile::tempdir().expect("tempdir");
    let core = dir.path().join("core.db");
    let saved = dir.path().join("saved-search.db");
    let mut h = harness(&core);
    let mut words = write_rows(&mut h, "lpr", 1, 10);
    // The file, closed (its pages are in the main file), copied aside.
    let why = h
        .search_open_after(|path| {
            std::fs::copy(path, &saved).expect("copy");
        })
        .expect("open");
    assert_eq!(why, None, "the file belongs");
    // A row is deleted and more are written; the flush applies them and the core prunes.
    let victim = words
        .iter()
        .find(|(_, w)| w.as_str() == "lpr3q")
        .map(|(id, _)| *id)
        .expect("victim");
    h.connection()
        .execute("DELETE FROM messages WHERE id = ?1", [victim.0])
        .expect("delete");
    words.remove(&victim);
    words.extend(write_rows(&mut h, "lpr", 11, 14));
    assert_eq!(
        search::queued(h.connection()).expect("queue"),
        0,
        "the log was pruned past the old copy's applied"
    );
    assert!(
        search::ids_with(h.search_db(), "lpr3q")
            .expect("match")
            .is_empty()
    );
    // The old copy is put back.
    let why = h
        .search_open_after(|path| {
            remove_search(path);
            std::fs::copy(&saved, path).expect("put back");
        })
        .expect("open");
    assert_eq!(why, Some(search::Rebuild::LogPruned));
    rebuilt_holds(&mut h, &words, &["lpr3q"]);
}

/// The same restore with rows still WAITING in core.db's log (not an empty log): the log's
/// first row is above the old copy's `applied` + 1, so the stretch between was pruned and the
/// file is built again (`LogPruned`); the deleted word is gone, the pruned stretch's rows and
/// the waiting rows are found (re-check of review D1).
#[test]
fn an_older_search_db_beside_a_log_with_waiting_rows_builds_the_file_again() {
    let dir = tempfile::tempdir().expect("tempdir");
    let core = dir.path().join("core.db");
    let saved = dir.path().join("saved-search.db");
    let mut h = harness(&core);
    let mut words = write_rows(&mut h, "lpw", 1, 10);
    let why = h
        .search_open_after(|path| {
            std::fs::copy(path, &saved).expect("copy");
        })
        .expect("open");
    assert_eq!(why, None, "the file belongs");
    let victim = words
        .iter()
        .find(|(_, w)| w.as_str() == "lpw3q")
        .map(|(id, _)| *id)
        .expect("victim");
    h.connection()
        .execute("DELETE FROM messages WHERE id = ?1", [victim.0])
        .expect("delete");
    words.remove(&victim);
    // Flushed and pruned: the stretch the old copy never saw is gone from the log.
    words.extend(write_rows(&mut h, "lpw", 11, 14));
    // Two rows wait in the log, above the pruned stretch.
    h.apply(vec![BackendEvent::Message(row(15, "lpwwaitaq"))])
        .expect("apply");
    h.apply(vec![BackendEvent::Message(row(16, "lpwwaitbq"))])
        .expect("apply");
    assert_eq!(search::queued(h.connection()).expect("queue"), 2);
    let why = h
        .search_open_after(|path| {
            remove_search(path);
            std::fs::copy(&saved, path).expect("put back");
        })
        .expect("open");
    assert_eq!(why, Some(search::Rebuild::LogPruned));
    for (n, w) in [(15, "lpwwaitaq"), (16, "lpwwaitbq")] {
        let id: i64 = h
            .connection()
            .query_row(
                "SELECT id FROM messages WHERE net_local = ?1",
                [n.to_string()],
                |r| r.get(0),
            )
            .expect("row");
        words.insert(MessageId(id), w.to_string());
    }
    rebuilt_holds(&mut h, &words, &["lpw3q"]);
}

/// A file with rows still waiting in the log belongs: the check is about rows that are GONE
/// from the log, not rows that are there.
#[test]
fn a_file_with_rows_waiting_in_the_log_is_kept() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = harness(&dir.path().join("core.db"));
    let words = write_rows(&mut h, "wait", 1, 10);
    h.apply(vec![BackendEvent::Message(row(11, "waitelevenq"))])
        .expect("apply");
    h.apply(vec![BackendEvent::Message(row(12, "waittwelveq"))])
        .expect("apply");
    assert_eq!(search::queued(h.connection()).expect("queue"), 2);
    assert_eq!(h.search_open().expect("open"), None);
    assert_eq!(meta(&h, "applied"), 10);
    let done = h.search_flush().expect("flush");
    assert_eq!((done.taken, done.indexed), (2, 2));
    assert_eq!(
        search::indexed(h.search_db()).expect("docsize").len(),
        words.len() + 2
    );
}

/// An error of the open checks that is not core.db's builds the file again (review D3): a
/// `meta` without its version row, an epoch that is not text, a `meta` whose column is not
/// the one this build reads. The search becomes ready after the rebuild.
#[test]
fn a_file_whose_numbers_or_tables_do_not_read_is_built_again() {
    for (case, expect) in [
        ("no version row", search::Rebuild::Version),
        ("integer epoch", search::Rebuild::Unjudged),
        ("renamed column", search::Rebuild::Unjudged),
        ("text floor", search::Rebuild::Unjudged),
        ("text compacted_for", search::Rebuild::Unjudged),
    ] {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut h = harness(&dir.path().join("core.db"));
        let words = write_rows(&mut h, "unj", 1, 12);
        let why = h
            .search_open_after(|path| {
                let conn = kourier_db::open(path, &kourier_db::DbKey::from(KEY)).expect("open");
                match case {
                    "no version row" => conn
                        .execute_batch("DELETE FROM meta WHERE key = 'version'")
                        .expect("delete"),
                    "integer epoch" => conn
                        .execute_batch(
                            "DROP TABLE epoch; CREATE TABLE epoch(value); INSERT INTO epoch VALUES (5);",
                        )
                        .expect("epoch"),
                    "text floor" | "text compacted_for" => {
                        let key = case.trim_start_matches("text ");
                        conn.execute(
                            "INSERT INTO meta(key, value) VALUES (?1, 'x')
                               ON CONFLICT(key) DO UPDATE SET value = 'x'",
                            [key],
                        )
                        .expect("text value");
                    }
                    _ => conn
                        .execute_batch("ALTER TABLE meta RENAME COLUMN value TO val")
                        .expect("rename"),
                }
            })
            .expect("the open does not fail");
        assert_eq!(why, Some(expect), "{case}");
        rebuilt_holds(&mut h, &words, &[]);
    }
}

/// The core never starts with one key for core.db and search.db (review P4): the
/// configuration's own check, before any file is opened.
#[tokio::test]
async fn the_core_refuses_one_key_for_core_db_and_search_db() {
    let dir = tempfile::tempdir().expect("tempdir");
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).expect("data dir");
    let cfg = CoreConfig::new(
        data.join("core.db"),
        kourier_db::DbKey::from([4; 32]),
        kourier_db::DbKey::from([4; 32]),
        NetworkRegistry::new(),
        Arc::new(DirVault::new(data.clone())),
        Arc::new(SystemClock),
        tokio::runtime::Handle::current(),
    );
    match Core::start(cfg).await {
        Err(CoreError::InvalidInput(_)) => {}
        Err(e) => panic!("another refusal: {e}"),
        Ok(_) => panic!("the core started with one key for both files"),
    }
    assert!(!data.join("core.db").exists(), "nothing was opened");
}
