//! The receive path under kills and failing zones, without a client (W3a, chunk C4; decision D3).
//!
//! A CHILD process (this test binary again, `--exact receive_kill::child`) plays upstream's pipeline
//! for hand-built batches, in upstream's order (`message/commit_batch.rs`): the stanza's pending row
//! (`store_pending_inbound[_batch]`), the hook ([`Hook::commit`]), the cleanup of the pending row
//! (`delete_pending_inbound_if`, a compare-and-swap) and only then the acknowledgement, which the child
//! stands in for with a line `ack <batch>` in a file, written and synced. A batch the hook refused is
//! sent again, as the server does for a message it was never told about. Between the batches stage 2
//! ([`Stage2::step`]) turns the inbox rows into events.
//!
//! The parent loops `KOURIER_FAILPOINT=<point>:<n>` for n = 1, 2, ... until the child exits normally
//! (an abort, like a power cut: no destructor, no rollback). After every kill it holds two things to
//! the files:
//!
//! 1. every batch the child had ACKNOWLEDGED is durable (all its messages are in the inbox): a hook that
//!    says `Ok` before COMMIT, or ignores an error, loses a message the server will never send again;
//! 2. after a second child (no point) has done what the server and the restart do, that is, sent every
//!    batch that was not acknowledged and run stage 2 to the end, every message is in `core_outbox`
//!    exactly once, every inbox row is `done` and no pending row is left.
//!
//! The seam tests do the same with a zone that FAILS instead of dying ([`crate::fault`]): the child
//! stays alive, the hook must answer `Err`, and the batch must land once when it comes again.
//!
//! The points that fire only over the Wire (`wa_inside_sessions_batch`, the prekey removal) are chunk
//! C10's loop; nothing here opens a client or a socket.
#![allow(
    clippy::await_holding_lock,
    reason = "the seam is process-wide: a test holds fault::serial() for its whole run"
)]

use std::collections::BTreeSet;
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use kourier_backend_api::{BackendEvent, FakeClock, decode_outbox_row};
use kourier_db::rusqlite::Connection;
use kourier_db::{DbKey, Priority, outbox};
use tokio::sync::Notify;
use wacore::store::traits::{
    PendingInboundKey, PendingInboundLease, PendingInboundMutation, PendingInboundRevision,
    PendingInboundRow, ProtocolStore,
};
use wacore::types::events::InboundMessage;
use wacore::types::message::{MessageInfo, MessageSource};
use waproto::whatsapp as wa;

use crate::error::sql;
use crate::fault;
use crate::hook::{Hook, Notes};
use crate::inbox::{Committed, Inbox};
use crate::kill_log;
use crate::stage2::{Stage2, Timing};
use crate::store::{KourierStore, StoreOwner};

const MODE: &str = "WA_RK_MODE";
const DIR: &str = "WA_RK_DIR";
const KEY: [u8; 32] = [11; 32];
const START: u64 = 1_700_000_300_000;
/// The most hits of one point any run here makes. A loop that goes past it is a bug.
const MAX_HITS: u32 = 12;
/// The most times a refused batch is sent again.
const MAX_SENDS: usize = 6;

/// One batch as upstream hands it to the hook: a live message (a batch of one) or a piece of the
/// offline drain.
struct Spec {
    chat: &'static str,
    sender: &'static str,
    ids: &'static [&'static str],
    offline: bool,
}

const BATCHES: [Spec; 4] = [
    Spec {
        chat: "100@g.us",
        sender: "200@lid",
        ids: &["b0m0"],
        offline: false,
    },
    Spec {
        chat: "100@g.us",
        sender: "201@lid",
        ids: &["b1m0", "b1m1", "b1m2"],
        offline: true,
    },
    Spec {
        chat: "300@lid",
        sender: "300@lid",
        ids: &["b2m0"],
        offline: false,
    },
    Spec {
        chat: "300@lid",
        sender: "300@lid",
        ids: &["b3m0", "b3m1"],
        offline: true,
    },
];

fn all_ids() -> Vec<String> {
    let mut ids: Vec<String> = BATCHES
        .iter()
        .flat_map(|b| b.ids.iter().map(|id| (*id).to_owned()))
        .collect();
    ids.sort();
    ids
}

fn item(spec: &Spec, id: &str) -> InboundMessage {
    let mut message = wa::Message::default();
    message.conversation = Some(format!("synthetic text {id}"));
    InboundMessage::builder()
        .message(Arc::new(message))
        .info(Arc::new(MessageInfo {
            id: id.into(),
            source: MessageSource {
                chat: spec.chat.parse().expect("jid"),
                sender: spec.sender.parse().expect("jid"),
                is_group: spec.chat.ends_with("@g.us"),
                ..MessageSource::default()
            },
            is_offline: spec.offline,
            ..MessageInfo::default()
        }))
        .build()
}

/// The stanza bytes upstream keeps in the pending row.
fn stanza(id: &str) -> Vec<u8> {
    format!("stanza {id}").into_bytes()
}

fn lease() -> PendingInboundLease {
    PendingInboundLease::new(Arc::new(()))
}

// ---------------------------------------------------------------------------------------------
// The child: upstream's pipeline for the hand-built batches.
// ---------------------------------------------------------------------------------------------

struct Pipeline {
    store: KourierStore,
    hook: Hook,
    stage2: Arc<Stage2>,
    events: PathBuf,
}

impl Pipeline {
    /// One line in the events file, written and synced: a killed child's lines survive it ([`kill_log`]).
    fn note(&self, line: &str) {
        kill_log::note(&self.events, line);
    }

    /// Upstream's commit of batch `i`: pending row, hook, cleanup, ack. A refusal by the hook
    /// (`Err`) is followed by the same batch again.
    async fn deliver(&self, i: usize) {
        let spec = &BATCHES[i];
        let batch: Vec<InboundMessage> = spec.ids.iter().map(|id| item(spec, id)).collect();
        for _ in 0..MAX_SENDS {
            let bytes: Vec<Vec<u8>> = spec.ids.iter().map(|id| stanza(id)).collect();
            if let [id] = spec.ids {
                self.store
                    .store_pending_inbound(spec.chat, spec.sender, id, &bytes[0])
                    .await
                    .expect("store the pending row");
            } else {
                let rows: Vec<PendingInboundRow<'_>> = spec
                    .ids
                    .iter()
                    .zip(&bytes)
                    .map(|(id, message)| PendingInboundRow {
                        chat: spec.chat,
                        sender: spec.sender,
                        id,
                        message,
                    })
                    .collect();
                self.store
                    .store_pending_inbound_batch(&rows)
                    .await
                    .expect("store the pending rows");
            }
            if self.hook.commit(&batch).await.is_err() {
                self.note(&format!("err {i}"));
                continue;
            }
            for (id, message) in spec.ids.iter().zip(&bytes) {
                let key = PendingInboundKey {
                    chat: spec.chat,
                    sender: spec.sender,
                    id,
                };
                let outcome = self
                    .store
                    .delete_pending_inbound_if(
                        key,
                        PendingInboundRevision::for_bytes(message),
                        4096,
                        lease(),
                    )
                    .await
                    .expect("clean up the pending row");
                assert!(matches!(outcome, PendingInboundMutation::Applied));
            }
            self.note(&format!("ack {i}"));
            return;
        }
        panic!("batch {i} was refused {MAX_SENDS} times");
    }

    /// One stage-2 step, again after a failed one.
    async fn step_once(&self) {
        for _ in 0..MAX_SENDS {
            match self.stage2.step().await {
                Ok(_) => return,
                Err(_) => self.note("step-err"),
            }
        }
        panic!("stage 2 kept failing");
    }

    /// Steps until nothing is left to take.
    async fn step_all(&self) {
        for _ in 0..(MAX_SENDS * 4) {
            match self.stage2.step().await {
                Ok(report) if report.taken == 0 => return,
                Ok(_) => {}
                Err(_) => self.note("step-err"),
            }
        }
        panic!("stage 2 did not settle");
    }
}

/// The child: `first` plays the pipeline from the start, `recover` does what the server and the restart
/// do after a death. Does nothing when it runs as an ordinary test.
#[test]
fn child() {
    let (Ok(mode), Ok(dir)) = (std::env::var(MODE), std::env::var(DIR)) else {
        return;
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let dir = PathBuf::from(dir);
        let owner = StoreOwner::open(&dir.join("backend.db"), &DbKey::from(KEY), "wa-rk")
            .expect("open the store");
        let clock = FakeClock::new(START);
        let stage2 = Arc::new(Stage2::new(
            owner.store(),
            Arc::new(clock.clone()),
            Arc::new(Notify::new()),
            Timing::default(),
        ));
        let inbox = Inbox::new(owner.store(), Arc::new(clock));
        let pipeline = Pipeline {
            store: owner.store(),
            hook: Hook::new(inbox, stage2.clone() as Arc<dyn Notes>),
            stage2,
            events: dir.join("events"),
        };
        match mode.as_str() {
            "first" => {
                pipeline.deliver(0).await;
                pipeline.deliver(1).await;
                pipeline.step_once().await;
                pipeline.deliver(2).await;
                pipeline.step_once().await;
                pipeline.deliver(3).await;
                pipeline.step_all().await;
            }
            "recover" => {
                let acked = acked(&dir);
                for i in (0..BATCHES.len()).filter(|i| !acked.contains(i)) {
                    pipeline.deliver(i).await;
                }
                pipeline.step_all().await;
            }
            other => panic!("unknown mode {other}"),
        }
        owner.close();
    });
}

// ---------------------------------------------------------------------------------------------
// The parent.
// ---------------------------------------------------------------------------------------------

fn lines(dir: &Path) -> Vec<String> {
    kill_log::lines(&dir.join("events"))
}

/// The batches whose acknowledgement the child had written.
fn acked(dir: &Path) -> BTreeSet<usize> {
    lines(dir)
        .iter()
        .filter_map(|l| l.strip_prefix("ack ")?.parse().ok())
        .collect()
}

fn reopen(dir: &Path) -> Connection {
    kourier_db::open(&dir.join("backend.db"), &DbKey::from(KEY)).expect("reopen with the key")
}

fn count(conn: &Connection, sql_text: &str) -> i64 {
    conn.query_row(sql_text, [], |r| r.get(0)).expect("count")
}

/// Runs the child on `dir`; `env` arms a point or the seam. Returns how it ended and what it said.
fn run_child(
    dir: &Path,
    mode: &str,
    env: Option<(&str, String)>,
) -> (std::process::ExitStatus, String) {
    let mut cmd = Command::new(std::env::current_exe().expect("test binary"));
    cmd.args([
        "--exact",
        "receive_kill::child",
        "--nocapture",
        "--test-threads=1",
    ])
    .env(MODE, mode)
    .env(DIR, dir)
    .env_remove("KOURIER_FAILPOINT")
    .env_remove(fault::ENV);
    if let Some((name, value)) = env {
        cmd.env(name, value);
    }
    let out = cmd.output().expect("spawn the child");
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status, said)
}

/// Everything the acknowledged batches promised: their messages are in the inbox and their pending
/// rows are gone.
fn assert_acked_are_durable(dir: &Path, what: &str) {
    let conn = reopen(dir);
    for i in acked(dir) {
        for id in BATCHES[i].ids {
            let rows: i64 = conn
                .query_row("SELECT count(*) FROM inbox WHERE id = ?1", [id], |r| {
                    r.get(0)
                })
                .expect("count rows");
            assert_eq!(
                rows, 1,
                "{what}: batch {i} was acknowledged, but message {id} is not in the inbox"
            );
        }
    }
}

/// The end state: each message once in `core_outbox`, every inbox row done, nothing pending.
fn assert_final(dir: &Path, what: &str) {
    let conn = reopen(dir);
    let mut locals: Vec<String> = Vec::new();
    for row in outbox::peek(&conn, 10_000).expect("peek the outbox") {
        for event in decode_outbox_row(&row.envelope).expect("decodes") {
            if let BackendEvent::Message(m) = event {
                locals.push(m.local.to_string());
            }
        }
    }
    locals.sort();
    assert_eq!(
        locals,
        all_ids(),
        "{what}: every message is in core_outbox exactly once"
    );
    let total = i64::try_from(all_ids().len()).expect("small");
    assert_eq!(
        (
            count(&conn, "SELECT count(*) FROM inbox WHERE state = 'new'"),
            count(&conn, "SELECT count(*) FROM inbox WHERE state = 'done'"),
        ),
        (0, total),
        "{what}: every inbox row is done"
    );
    assert_eq!(
        count(&conn, "SELECT count(*) FROM pending_inbound_messages"),
        0,
        "{what}: no pending row is left"
    );
}

/// Kills the pipeline at `point:1`, `point:2`, ... until the child ends normally; returns how many
/// kills there were.
fn kill_at_every_hit(point: &str) -> u32 {
    let mut kills = 0;
    for n in 1..=MAX_HITS + 1 {
        assert!(n <= MAX_HITS, "{point} is hit more than {MAX_HITS} times");
        let dir = tempfile::tempdir().expect("temp dir");
        let what = format!("{point}:{n}");
        let (status, said) = run_child(
            dir.path(),
            "first",
            Some(("KOURIER_FAILPOINT", what.clone())),
        );
        if status.success() {
            assert_final(dir.path(), &what);
            break;
        }
        assert_eq!(
            status.signal(),
            Some(6),
            "{what} must abort: {status:?} {said}"
        );
        assert!(said.contains("hit: aborting"), "{what}: {said}");
        assert_acked_are_durable(dir.path(), &what);
        let (status, said) = run_child(dir.path(), "recover", None);
        assert!(status.success(), "{what}: the recovery failed: {said}");
        assert_final(dir.path(), &format!("{what} after the recovery"));
        kills += 1;
    }
    assert!(
        kills >= 1,
        "{point} was never hit: the kill test saw nothing"
    );
    kills
}

#[test]
fn every_message_once_after_a_kill_in_the_pending_writes() {
    use crate::failpoints as fp;
    // Four stanzas stored (two single, two batches); seven cleanups (one per message).
    assert_eq!(kill_at_every_hit(fp::INSIDE_PENDING_STORE), 4);
    assert_eq!(kill_at_every_hit(fp::AFTER_PENDING_STORE), 4);
    assert_eq!(kill_at_every_hit(fp::INSIDE_PENDING_CAS), 7);
    assert_eq!(kill_at_every_hit(fp::AFTER_PENDING_CAS), 7);
}

#[test]
fn every_message_once_after_a_kill_in_the_inbox_commit() {
    use crate::failpoints as fp;
    // One zone per batch, whatever its size.
    assert_eq!(kill_at_every_hit(fp::INSIDE_INBOX_COMMIT), 4);
    assert_eq!(kill_at_every_hit(fp::AFTER_INBOX_COMMIT), 4);
}

#[test]
fn every_message_once_after_a_kill_in_stage_2() {
    use crate::failpoints as fp;
    // Three steps take rows (after batch 1, after batch 2, the last), one zone each.
    assert_eq!(kill_at_every_hit(fp::INSIDE_STAGE2), 3);
    assert_eq!(kill_at_every_hit(fp::AFTER_STAGE2), 3);
}

/// Fails the n-th zone of `seam` for n = 1, 2, ... (the child lives on); returns how many failures
/// there were and the lines of each run.
fn fail_every_zone(seam: &str, marker: &str) -> Vec<Vec<String>> {
    let mut failed = Vec::new();
    for n in 1..=MAX_HITS + 1 {
        assert!(n <= MAX_HITS, "{seam} is hit more than {MAX_HITS} times");
        let dir = tempfile::tempdir().expect("temp dir");
        let what = format!("{seam}:{n}");
        let (status, said) = run_child(dir.path(), "first", Some((fault::ENV, what.clone())));
        assert!(status.success(), "{what}: the child died: {said}");
        assert_final(dir.path(), &what);
        let log = lines(dir.path());
        if !log.iter().any(|l| l.starts_with(marker)) {
            break;
        }
        failed.push(log);
    }
    failed
}

#[test]
fn a_failed_inbox_zone_is_answered_with_an_error_and_the_batch_lands_once_when_it_comes_again() {
    let failed = fail_every_zone(fault::INBOX_COMMIT, "err ");
    assert_eq!(failed.len(), 4, "one failing zone per batch was seen");
    for (n, log) in failed.iter().enumerate() {
        let refused = log
            .iter()
            .position(|l| *l == format!("err {n}"))
            .expect("the hook refused the batch");
        let acked = log
            .iter()
            .position(|l| *l == format!("ack {n}"))
            .expect("the batch was acknowledged in the end");
        assert!(
            refused < acked,
            "batch {n} was acknowledged before its commit had succeeded: {log:?}"
        );
    }
}

#[test]
fn a_failed_stage_2_zone_leaves_the_rows_and_the_next_step_finishes_them() {
    let failed = fail_every_zone(fault::STAGE2_COMMIT, "step-err");
    assert_eq!(failed.len(), 3, "one failing zone per step was seen");
}

// ---------------------------------------------------------------------------------------------
// The lane: the hook's commit and upstream's cleanup of the pending row.
// ---------------------------------------------------------------------------------------------

/// Parks the store's writer thread until the returned sender is used: every job queued meanwhile waits.
async fn park(store: &KourierStore) -> std::sync::mpsc::Sender<()> {
    let (release, wait) = std::sync::mpsc::channel::<()>();
    let (started_tx, started) = std::sync::mpsc::channel::<()>();
    let store = store.clone();
    tokio::spawn(async move {
        let _ = store
            .raw(Priority::High, move |_| {
                let _ = started_tx.send(());
                let _ = wait.recv();
                Ok(())
            })
            .await;
    });
    for _ in 0..500 {
        if started.try_recv().is_ok() {
            return release;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("the writer did not park");
}

/// Upstream deletes the pending row with a compare-and-swap job it queues AFTER the hook returned;
/// the writer is first in, first out per lane and priority, so the hook's own commit cannot be behind
/// it, but only while both are the same lane and priority. If the inbox zone ever moved to the low
/// priority (a sweep), the cleanup would run first and a kill between the two would lose the message:
/// the pending row gone, the inbox row never written. The test queues both while the writer is parked
/// and lets a trigger on the pending row's deletion record how many inbox rows exist at that moment.
#[tokio::test]
async fn the_hooks_commit_is_ahead_of_upstreams_cleanup_of_the_pending_row() {
    #[derive(Default)]
    struct Quiet;
    impl Notes for Quiet {
        fn committed(&self, _: &Committed) {}
        fn failed(&self) {}
    }
    let _serial = fault::serial();
    let dir = tempfile::tempdir().expect("temp dir");
    let owner = StoreOwner::open(&dir.path().join("backend.db"), &DbKey::from(KEY), "wa-lane")
        .expect("open");
    let store = owner.store();
    let spec = &BATCHES[0];
    store
        .store_pending_inbound(spec.chat, spec.sender, spec.ids[0], &stanza(spec.ids[0]))
        .await
        .expect("pending row");
    store
        .raw(Priority::High, |conn| {
            conn.execute_batch(
                "CREATE TABLE probe(inbox_rows INTEGER NOT NULL); \
                 CREATE TRIGGER probe_after_delete AFTER DELETE ON pending_inbound_messages \
                 BEGIN INSERT INTO probe SELECT count(*) FROM inbox; END;",
            )
            .map_err(sql("create the probe"))
        })
        .await
        .expect("probe");
    let hook = Hook::new(
        Inbox::new(owner.store(), Arc::new(FakeClock::new(START))),
        Arc::new(Quiet),
    );
    let release = park(&store).await;
    let batch = [item(spec, spec.ids[0])];
    let mut commit = Box::pin(hook.commit(&batch));
    assert!(futures::poll!(commit.as_mut()).is_pending());
    let key = PendingInboundKey {
        chat: spec.chat,
        sender: spec.sender,
        id: spec.ids[0],
    };
    let mut cleanup = Box::pin(store.delete_pending_inbound_if(
        key,
        PendingInboundRevision::for_bytes(&stanza(spec.ids[0])),
        4096,
        lease(),
    ));
    assert!(futures::poll!(cleanup.as_mut()).is_pending());
    release.send(()).expect("release");
    let (committed, cleaned) = tokio::join!(commit, cleanup);
    committed.expect("the hook committed");
    assert!(matches!(
        cleaned.expect("cleanup"),
        PendingInboundMutation::Applied
    ));
    let seen: Vec<i64> = store
        .read(|conn| {
            crate::store::rows(
                conn,
                "read the probe",
                "SELECT inbox_rows FROM probe",
                [],
                |r| r.get(0),
            )
        })
        .await
        .expect("probe rows");
    assert_eq!(
        seen,
        [1],
        "the pending row was deleted before the hook's commit was in the inbox"
    );
    owner.close();
}
