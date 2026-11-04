//! `core_outbox` of the Telegram backend: `AccountReady` and the user id commit together, rows
//! ship once and leave only after the core's ack, refused and undecodable rows are quarantined, a
//! dropped or cancelled pass keeps the rows, and a kill at either kill point leaves the state whole
//! (critic M9: a CHILD process, this test binary re-executed by `kourier_tdlib::spawn::spawn_self`,
//! is aborted at the failpoint; the parent reopens the file).
//!
//! Every value is synthetic (the test number of the decisions, the name "Ada Test").

use std::io::{Read as _, Write as _};
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};

use kourier_backend_api::{BackendEvent, Envelope};
use kourier_db::DbKey;
use kourier_db::rusqlite::Connection;
use kourier_model::{PersistError, Redacted, UserId};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::*;
use crate::db::{self, AccountRow};
use crate::parts::TgEnv;

const USER: i64 = 100;

fn key() -> DbKey {
    DbKey::from_bytes([7; 32])
}

fn path(dir: &Path) -> PathBuf {
    dir.join("backend.db")
}

/// The account row of a login that has not reached Ready.
fn fresh(dir: &Path) -> Connection {
    let conn = db::open(&path(dir), &key()).expect("open");
    db::create_account(
        &conn,
        &AccountRow {
            env: TgEnv::Test,
            user_id: None,
            pin: crate::PIN.commit.to_owned(),
            created_ms: 1,
        },
    )
    .expect("create");
    conn
}

fn ready_event() -> BackendEvent {
    BackendEvent::AccountReady {
        own_ids: vec![
            UserId::parse("tg:100").expect("tg id"),
            UserId::parse("pn:999068204719").expect("pn id"),
        ],
        device: None,
        display_name: Some(Redacted::new("Ada Test".to_owned())),
        number: Some(Redacted::new("+999068204719".to_owned())),
    }
}

fn rows(conn: &Connection) -> u64 {
    kourier_db::outbox::len(conn).expect("len")
}

fn poison(conn: &Connection) -> u64 {
    kourier_db::outbox::poison_len(conn).expect("poison len")
}

fn user_id(conn: &Connection) -> Option<i64> {
    db::account(conn).expect("read").and_then(|r| r.user_id)
}

/// The events of every waiting row, in order.
fn waiting(conn: &Connection) -> Vec<BackendEvent> {
    kourier_db::outbox::peek(conn, 100)
        .expect("peek")
        .iter()
        .flat_map(|r| kourier_backend_api::decode_outbox_row(&r.envelope).expect("decode"))
        .collect()
}

/// Appends raw bytes as a row (a row this build cannot decode).
fn append_raw(conn: &Connection, bytes: &[u8]) -> i64 {
    let tx = conn.unchecked_transaction().expect("tx");
    let seq = kourier_db::outbox::append(&tx, bytes, 1).expect("append");
    tx.commit().expect("commit");
    seq
}

/// A fake core: answers the n-th envelope with `answers[n]` (the last one repeats) and returns
/// every envelope's events, until the channel closes.
async fn core(
    mut rx: mpsc::Receiver<Envelope>,
    answers: Vec<Result<(), PersistError>>,
) -> Vec<Vec<BackendEvent>> {
    let mut seen = Vec::new();
    while let Some(envelope) = rx.recv().await {
        let answer = answers[seen.len().min(answers.len() - 1)].clone();
        seen.push(envelope.events);
        envelope.ack.expect("a durable envelope").complete(answer);
    }
    seen
}

fn refused(retryable: bool) -> Result<(), PersistError> {
    Err(PersistError {
        retryable,
        detail: "synthetic refusal".to_owned(),
    })
}

/// One pass against the fake core; the sender is dropped after the pass so the core ends.
async fn pass(
    conn: &Connection,
    answers: Vec<Result<(), PersistError>>,
) -> (Shipped, Vec<Vec<BackendEvent>>) {
    let (tx, rx) = mpsc::channel(4);
    let cancel = CancellationToken::new();
    let ship = async {
        let shipped = ship_pending(conn, &tx, &cancel).await;
        drop(tx);
        shipped
    };
    tokio::join!(ship, core(rx, answers))
}

#[test]
fn account_ready_and_the_user_id_commit_together() {
    let dir = tempfile::tempdir().expect("tempdir");
    let conn = fresh(dir.path());
    let seq = append_ready(&conn, USER, &ready_event(), 5).expect("append");
    assert!(seq > 0, "no seq");
    assert_eq!(user_id(&conn), Some(USER), "the user id was not set");
    assert_eq!(
        waiting(&conn),
        vec![ready_event()],
        "the row is not AccountReady"
    );

    // Another user for the same account row: refused, NOTHING written (no second row).
    assert!(
        append_ready(&conn, USER + 1, &ready_event(), 6).is_err(),
        "another user id was accepted"
    );
    assert_eq!(rows(&conn), 1, "a refused AccountReady left a row");
    assert_eq!(user_id(&conn), Some(USER));

    // No account row: refused, no row.
    let other = tempfile::tempdir().expect("tempdir");
    let bare = db::open(&path(other.path()), &key()).expect("open");
    assert!(append_ready(&bare, USER, &ready_event(), 5).is_err());
    assert_eq!(rows(&bare), 0, "AccountReady without the account row");
}

#[tokio::test(flavor = "current_thread")]
async fn a_pending_row_ships_once_and_is_deleted_after_the_ack() {
    let dir = tempfile::tempdir().expect("tempdir");
    let conn = fresh(dir.path());
    let seq = append_ready(&conn, USER, &ready_event(), 5).expect("append");

    let (tx, mut rx) = mpsc::channel::<Envelope>(4);
    let cancel = CancellationToken::new();
    let check = async {
        let envelope = rx.recv().await.expect("an envelope");
        assert_eq!(envelope.events, vec![ready_event()]);
        // Still there while the core has not committed.
        assert_eq!(rows(&conn), 1, "deleted before the ack");
        envelope.ack.expect("durable").complete(Ok(()));
        rx
    };
    let (shipped, mut rx) = tokio::join!(ship_pending(&conn, &tx, &cancel), check);
    assert_eq!(
        shipped,
        Shipped {
            acked: Some(seq),
            quarantined: 0,
            end: ShipEnd::Drained,
        }
    );
    assert_eq!(rows(&conn), 0, "kept after the ack");

    // A second pass ships nothing.
    let again = ship_pending(&conn, &tx, &cancel).await;
    assert_eq!(again.acked, None);
    assert_eq!(again.end, ShipEnd::Drained);
    assert!(rx.try_recv().is_err(), "the acked row was shipped again");
}

#[tokio::test(flavor = "current_thread")]
async fn a_refused_row_is_quarantined_not_resent() {
    let dir = tempfile::tempdir().expect("tempdir");
    let conn = fresh(dir.path());
    append_ready(&conn, USER, &ready_event(), 5).expect("append");
    let (shipped, seen) = pass(&conn, vec![refused(false)]).await;
    assert_eq!(seen.len(), 1, "shipped again after a final refusal");
    assert_eq!(shipped.acked, None, "a refused row counted as acked");
    assert_eq!(shipped.quarantined, 1);
    assert_eq!(shipped.end, ShipEnd::Drained);
    assert_eq!((rows(&conn), poison(&conn)), (0, 1), "not quarantined");

    let (again, seen) = pass(&conn, vec![Ok(())]).await;
    assert!(seen.is_empty(), "the quarantined row was resent");
    assert_eq!(again.acked, None);
}

#[tokio::test(flavor = "current_thread")]
async fn a_retryable_refusal_ships_the_row_again() {
    let dir = tempfile::tempdir().expect("tempdir");
    let conn = fresh(dir.path());
    let seq = append_ready(&conn, USER, &ready_event(), 5).expect("append");
    let (shipped, seen) = pass(&conn, vec![refused(true), Ok(())]).await;
    assert_eq!(seen, vec![vec![ready_event()], vec![ready_event()]]);
    assert_eq!(shipped.acked, Some(seq));
    assert_eq!(shipped.quarantined, 0);
    assert_eq!((rows(&conn), poison(&conn)), (0, 0));
}

#[tokio::test(flavor = "current_thread")]
async fn an_undecodable_row_is_quarantined_and_the_next_one_ships() {
    let dir = tempfile::tempdir().expect("tempdir");
    let conn = fresh(dir.path());
    append_raw(&conn, &[9, 9, 9]);
    let seq = append_ready(&conn, USER, &ready_event(), 5).expect("append");
    let (shipped, seen) = pass(&conn, vec![Ok(())]).await;
    assert_eq!(
        seen,
        vec![vec![ready_event()]],
        "the junk row reached the core"
    );
    assert_eq!(shipped.acked, Some(seq));
    assert_eq!(shipped.quarantined, 1);
    assert_eq!((rows(&conn), poison(&conn)), (0, 1));
}

#[tokio::test(flavor = "current_thread")]
async fn a_row_left_by_a_crash_ships_at_the_next_start() {
    let dir = tempfile::tempdir().expect("tempdir");
    let seq = {
        let conn = fresh(dir.path());
        append_ready(&conn, USER, &ready_event(), 5).expect("append")
    };
    let conn = db::open(&path(dir.path()), &key()).expect("reopen");
    let (shipped, seen) = pass(&conn, vec![Ok(())]).await;
    assert_eq!(seen, vec![vec![ready_event()]]);
    assert_eq!(shipped.acked, Some(seq));
    assert_eq!(rows(&conn), 0);
    assert_eq!(user_id(&conn), Some(USER));
}

#[tokio::test(flavor = "current_thread")]
async fn a_gone_core_or_a_cancel_keeps_the_row() {
    let dir = tempfile::tempdir().expect("tempdir");
    let conn = fresh(dir.path());
    append_ready(&conn, USER, &ready_event(), 5).expect("append");

    let (tx, rx) = mpsc::channel(4);
    drop(rx);
    let cancel = CancellationToken::new();
    let gone = ship_pending(&conn, &tx, &cancel).await;
    assert_eq!(gone.end, ShipEnd::CoreGone);
    assert_eq!(rows(&conn), 1, "a gone core lost the row");

    let (tx, _rx) = mpsc::channel(4);
    cancel.cancel();
    let cancelled = ship_pending(&conn, &tx, &cancel).await;
    assert_eq!(cancelled.end, ShipEnd::Cancelled);
    assert_eq!(rows(&conn), 1, "a cancel lost the row");
}

/// Critic H1: the actor's `select!` may drop a pass while the core holds the envelope; the row
/// stays and the next pass ships it again.
#[tokio::test(flavor = "current_thread")]
async fn a_dropped_pass_leaves_the_row_for_the_next_one() {
    let dir = tempfile::tempdir().expect("tempdir");
    let conn = fresh(dir.path());
    let seq = append_ready(&conn, USER, &ready_event(), 5).expect("append");
    let (tx, mut rx) = mpsc::channel::<Envelope>(4);
    let cancel = CancellationToken::new();
    let held = tokio::select! {
        _ = ship_pending(&conn, &tx, &cancel) => panic!("the pass ended without an ack"),
        envelope = rx.recv() => envelope.expect("an envelope"),
    };
    // The pass is dropped now; the core acks too late (nobody waits for it).
    held.ack.expect("durable").complete(Ok(()));
    assert_eq!(rows(&conn), 1, "a dropped pass deleted the row");
    drop(tx);
    drop(rx);
    let (shipped, seen) = pass(&conn, vec![Ok(())]).await;
    assert_eq!(seen, vec![vec![ready_event()]]);
    assert_eq!(shipped.acked, Some(seq));
}

// ---- kill points (critic M9) --------------------------------------------------------------------

/// The test the child runs (`spawn_self` passes `--exact` with this name).
const KILL_CHILD: &str = "outbox::tests::kill_child";
/// The most hits of one failpoint `append_ready` makes. A loop that goes past it is a bug.
const MAX_HITS: u32 = 4;

/// The child: armed by `KOURIER_FAILPOINT` (the one variable `spawn_self` passes); reads the
/// directory from its socket on fd 3 and runs `append_ready` once. Does nothing in a normal run.
#[test]
fn kill_child() {
    if std::env::var_os(kourier_db::failpoint::ENV).is_none() {
        return;
    }
    let Some(mut sock) = kourier_tdlib::spawn::helper_socket() else {
        std::process::exit(2);
    };
    let mut dir = String::new();
    if sock.read_to_string(&mut dir).is_err() {
        std::process::exit(3);
    }
    let conn = db::open(&path(Path::new(&dir)), &key()).unwrap_or_else(|_| std::process::exit(4));
    if append_ready(&conn, USER, &ready_event(), 5).is_err() {
        std::process::exit(5);
    }
    std::process::exit(0);
}

/// How one child run ended: `true` = aborted at the failpoint, `false` = a normal exit.
fn run_child(dir: &Path, spec: &str) -> bool {
    let mut s = kourier_tdlib::spawn::spawn_self(KILL_CHILD, (kourier_db::failpoint::ENV, spec))
        .expect("spawn the child");
    s.sock
        .write_all(dir.as_os_str().as_encoded_bytes())
        .expect("send the directory");
    s.sock
        .shutdown(std::net::Shutdown::Write)
        .expect("end the directory");
    // Keep the child's stderr open until it ends (its abort line goes there); never shown.
    let mut err = Vec::new();
    if let Some(mut e) = s.stderr.take() {
        let _ = e.read_to_end(&mut err);
    }
    let status = s.child.wait().expect("wait for the child");
    match status.signal() {
        Some(libc::SIGABRT) => true,
        Some(_) => panic!("the child died by another signal"),
        None => {
            assert!(status.success(), "the child failed before the kill point");
            false
        }
    }
}

/// For each kill point and n = 1, 2, ... until the child exits normally: a fresh account, a child
/// killed at the n-th hit, then the state is exactly before (inside) or exactly after (after),
/// never in between.
#[test]
fn account_ready_is_whole_or_absent_at_every_kill_point() {
    if !kourier_db::failpoint::ENABLED {
        eprintln!("SKIPPED: failpoints are off in this build");
        return;
    }
    for (point, killed_is_after) in [(INSIDE_ACCOUNT_READY, false), (AFTER_ACCOUNT_READY, true)] {
        let mut kills = 0;
        for n in 1..=MAX_HITS + 1 {
            assert!(n <= MAX_HITS, "{point} was hit more often than expected");
            let dir = tempfile::tempdir().expect("tempdir");
            drop(fresh(dir.path()));
            let killed = run_child(dir.path(), &format!("{point}:{n}"));
            let conn = db::open(&path(dir.path()), &key()).expect("reopen");
            let after = (user_id(&conn), waiting(&conn));
            if killed && !killed_is_after {
                assert_eq!(
                    after,
                    (None, Vec::new()),
                    "torn or committed at {point}:{n}"
                );
            } else {
                assert_eq!(
                    after,
                    (Some(USER), vec![ready_event()]),
                    "not whole after {point}:{n}"
                );
            }
            if !killed {
                break;
            }
            kills += 1;
        }
        assert!(kills >= 1, "{point} was never hit");
    }
}
