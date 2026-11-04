//! The history under kills, without a client (W4 C22; decision D2 and the brief's section 5).
//!
//! A CHILD process (this test binary again, `--exact history::kill::child`) plays the whole road of a push
//! with hand-made chunks: four captures through [`Capture::commit`] (what the adapter does, one stage zone
//! each, a line `captured <i>` written and synced after each `Ok`: the point where upstream would answer the
//! phone), then the real [`Driver`] over the stage and `backend.db`, with a FAKE CORE that takes the rows of
//! `core_outbox` the way the forwarder and the core do: for every message of a history batch it writes a line
//! `msg <seq> <conv> <origin> <local>` to the events file, synced, and only then deletes the row and tells the
//! driver. The child ends when every chunk is retired and no row is left.
//!
//! The parent loops `KOURIER_FAILPOINT=<point>:<n>` for n = 1, 2, ... until the child exits normally (an abort,
//! like a power cut: no destructor, no rollback). The points are the history's eight: the capture zone, the
//! delivery row's zone, the zone that settles answered rows and the job that retires a delivered chunk, each
//! `INSIDE` (everything written, nothing committed) and `AFTER` (just committed). After a kill:
//!
//! 1. every chunk whose capture had returned `Ok` is in the stage: nothing was answered to the phone before it
//!    was durable;
//! 2. after a second child (no point) has done what the server and the restart do, that is, captured again the
//!    chunks whose capture never returned (the phone is told nothing and sends the notice again), and gone on
//!    from the books, the end state is the one an unkilled run reaches: every message the unkilled run gave the
//!    core is there, NO message under two different outbox seqs (a row shipped twice under ONE seq is allowed:
//!    the core takes it once), the cursor at the end, every chunk `done` in the stage with its bytes gone and
//!    retired in the books, the counts, the secrets and the kept items the same.
//!
//! The unkilled run is the REFERENCE: its end state is also held to the numbers the chunks were made with, so a
//! pipeline that drops a message in every run does not agree with itself.
//!
//! Two tests: the sample (one kill at each of the eight points, in the regular set) and the loop (every hit of
//! every point). Nothing here opens a WhatsApp client or a socket; every id and number is synthetic.

use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use kourier_backend_api::{BackendEvent, FakeClock, decode_outbox_row};
use kourier_db::rusqlite::Connection;
use kourier_db::{DbKey, outbox};
use tokio::sync::{Notify, watch};
use wacore::store::device::Device;
use wacore::store::traits::DeviceStore as _;
use waproto::buffa::MessageField;
use waproto::whatsapp as wa;

use super::capture::Capture;
use super::driver::{Driver, Parts, Timing};
use super::stage::{LazyStage, derive_key};
use super::testing::{Chat, Chunk, Msg, Status, SyncType};
use crate::error::{database, sql};
use crate::failpoints as fp;
use crate::fault;
use crate::kill_log;
use crate::stage2::{self, Stage2};
use crate::store::{KourierStore, StoreOwner};

const MODE: &str = "WA_HK_MODE";
const DIR: &str = "WA_HK_DIR";
const KEY: [u8; 32] = [14; 32];
const NOW: u64 = 1_800_000_000_000;
/// Our own account: the stage's owner is the LID user.
const PN: &str = "15550100123";
const LID: &str = "99900100123";
/// A hit count past this is a bug in the loop (the most any point is hit by one run).
const MAX_HITS: u32 = 60;
/// The most real time a child may take before it gives up (it exits with a failure, not an abort).
const CHILD_LIMIT: Duration = Duration::from_secs(60);

const PEER_A: &str = "15550100101@s.whatsapp.net";
const PEER_B: &str = "15550100102@s.whatsapp.net";
const PEER_C: &str = "15550100103@s.whatsapp.net";
const GROUP: &str = "120363000000000001@g.us";
const MEMBER: &str = "15550100104@s.whatsapp.net";
const SECRET_1: [u8; 32] = [0x31; 32];
const SECRET_2: [u8; 32] = [0x32; 32];
const SECRET_3: [u8; 32] = [0x33; 32];

/// The ids of the notices that announce the four chunks (the capture's key).
const NOTICES: [&str; 4] = [
    "3AC0FFEE0000E001",
    "3AC0FFEE0000E002",
    "3AC0FFEE0000E003",
    "3AC0FFEE0000E004",
];
/// The id of the one message a later step maps (a poll vote): kept, not delivered.
const DEFERRED_ID: &str = "V0";
/// How many messages the long chat has: two batches of the delivery (500 at most each).
const LONG: usize = 620;

/// The four chunks of the push, in the phone's order. Chunk 0 holds a chat of ours with an own message (a status
/// and a timer of its own), a reaction and two secrets, and a group; chunk 1 the long chat; chunk 2 more of the
/// first chat and a message that is kept; chunk 3, at 100 %, the older part of the group and the end.
fn chunks() -> [Chunk; 4] {
    [
        Chunk::new(SyncType::INITIAL_BOOTSTRAP, 0, 20)
            .push_name(PEER_A, "Alex Synthetic")
            .push_name(PEER_B, "Blair Synthetic")
            .chat(
                Chat::new(PEER_A)
                    .unread(2)
                    .active_at(1_700_000_300)
                    .message(
                        Msg::mine(PEER_A, "A3", 1_700_000_130, "three", Status::READ)
                            .timer(86_400, Some(1_700_000_130)),
                    )
                    .message(
                        Msg::text(PEER_A, "A2", 1_700_000_120, "two")
                            .reaction(Some(PEER_A), "+", 1_700_000_125_000)
                            .secret(&SECRET_1),
                    )
                    .message(Msg::text(PEER_A, "A1", 1_700_000_110, "one").secret(&SECRET_2)),
            )
            .chat(
                Chat::new(GROUP)
                    .name("A synthetic group")
                    .active_at(1_700_000_200)
                    .message(
                        Msg::text(GROUP, "G2", 1_700_000_160, "hello again")
                            .from(MEMBER)
                            .secret(&SECRET_3),
                    )
                    .message(Msg::text(GROUP, "G1", 1_700_000_150, "hello").from(MEMBER)),
            ),
        Chunk::new(SyncType::INITIAL_BOOTSTRAP, 1, 45).chat(
            Chat::new(PEER_B)
                .unread(5)
                .active_at(1_700_000_250)
                .texts("L", 1_700_001_000, LONG),
        ),
        Chunk::new(SyncType::INITIAL_BOOTSTRAP, 2, 70)
            .chat(
                Chat::new(PEER_A)
                    .active_at(1_700_000_300)
                    .message(Msg::text(PEER_A, "A0", 1_700_000_100, "zero")),
            )
            .chat(
                Chat::new(PEER_C)
                    .active_at(1_700_000_050)
                    .message(vote(PEER_C, DEFERRED_ID, 1_700_000_041))
                    .message(Msg::text(PEER_C, "C0", 1_700_000_040, "first")),
            ),
        Chunk::new(SyncType::INITIAL_BOOTSTRAP, 3, 100).chat(
            Chat::new(GROUP)
                .active_at(1_700_000_200)
                .end_of_history()
                .message(Msg::text(GROUP, "G0", 1_700_000_140, "first of the group").from(MEMBER)),
        ),
    ]
}

/// A poll vote: a kind the map keeps for a later build, delivered as nothing.
fn vote(chat: &str, id: &str, at: u64) -> Msg {
    let mut msg = Msg::text(chat, id, at, "");
    let mut content = wa::Message::default();
    content.poll_update_message = MessageField::some(wa::message::PollUpdateMessage::default());
    msg.info.message = MessageField::some(content);
    msg
}

/// The ids of every message of the four chunks that the core must be given.
fn expected_locals() -> BTreeSet<String> {
    chunks()
        .iter()
        .flat_map(|chunk| chunk.sync.conversations.iter())
        .flat_map(|chat| chat.messages.iter())
        .filter_map(|entry| entry.message.as_option()?.key.as_option()?.id.clone())
        .filter(|id| id != DEFERRED_ID)
        .collect()
}

fn device() -> Device {
    let mut device = Device::new();
    device.pn = Some(format!("{PN}:3@s.whatsapp.net").parse().expect("jid"));
    device.lid = Some(format!("{LID}@lid").parse().expect("jid"));
    device
}

// ---------------------------------------------------------------------------------------------
// The child.
// ---------------------------------------------------------------------------------------------

/// The events file: one line per fact, written and synced, so a killed child's lines survive it ([`kill_log`]).
struct Events(PathBuf);

impl Events {
    fn note(&self, line: &str) {
        kill_log::note(&self.0, line);
    }
}

/// The core: takes every row of `core_outbox`, writes what the history batches gave it, answers.
struct Core {
    store: KourierStore,
    events: Events,
    acked: watch::Sender<i64>,
}

impl Core {
    /// Answers every row there is.
    async fn answer(&self) {
        let rows = self
            .store
            .read(|conn| outbox::peek(conn, 1000).map_err(|_| database("peek")))
            .await
            .expect("peek the outbox");
        for row in rows {
            let seq = row.seq;
            for event in decode_outbox_row(&row.envelope).expect("the row decodes") {
                if let BackendEvent::HistoryBatch { conv, messages, .. } = event {
                    for m in &messages {
                        self.events.note(&format!(
                            "msg\t{seq}\t{conv:?}\t{:?}\t{}",
                            m.origin, m.local
                        ));
                    }
                }
            }
            self.store
                .write("fake core", move |tx| {
                    tx.execute("DELETE FROM core_outbox WHERE seq = ?1", [seq])
                        .map(drop)
                        .map_err(sql("answer"))
                })
                .await
                .expect("answer the row");
            self.acked.send_modify(|n| *n += 1);
        }
    }

    /// Every chunk is retired, no chat still owes its end (the push's end comes after the last retire, in a
    /// row of its own) and the core holds nothing.
    async fn finished(&self) -> bool {
        self.store
            .read(|conn| {
                let retired: i64 = conn
                    .query_row(
                        "SELECT count(*) FROM history_chunks WHERE retired = 1",
                        [],
                        |r| r.get(0),
                    )
                    .map_err(sql("retired"))?;
                let open: i64 = conn
                    .query_row(
                        "SELECT count(*) FROM history_chats WHERE more = 1",
                        [],
                        |r| r.get(0),
                    )
                    .map_err(sql("open"))?;
                let queued: i64 = conn
                    .query_row("SELECT count(*) FROM core_outbox", [], |r| r.get(0))
                    .map_err(sql("queued"))?;
                Ok(retired == 4 && open == 0 && queued == 0)
            })
            .await
            .expect("look at the books")
    }
}

/// The child: `first` plays the push from the start, `recover` does what the phone and the restart do after a
/// death. Does nothing when it runs as an ordinary test.
#[test]
fn child() {
    let (Ok(mode), Ok(dir)) = (std::env::var(MODE), std::env::var(DIR)) else {
        return;
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(play(&mode, PathBuf::from(dir)));
}

async fn play(mode: &str, dir: PathBuf) {
    let backend_db = dir.join("backend.db");
    let owner = StoreOwner::open(&backend_db, &DbKey::from(KEY), "wa-hk").expect("open the store");
    let store = owner.store();
    match mode {
        "first" => store.save(&device()).await.expect("save the device"),
        "recover" => {}
        other => panic!("unknown mode {other}"),
    }
    let events = Events(dir.join("events"));
    let wake = Arc::new(Notify::new());
    let stage = Arc::new(LazyStage::new(&backend_db, &DbKey::from(KEY)));
    let capture = Capture::new(stage.clone(), wake.clone());
    // The phone sends again what it was never told it arrived.
    let captured = captured(&dir);
    for (i, chunk) in chunks().iter().enumerate() {
        if captured.contains(&i) {
            continue;
        }
        capture
            .commit(
                LID,
                NOTICES[i],
                Some(SyncType::INITIAL_BOOTSTRAP as i32),
                chunk.blob(),
            )
            .await
            .expect("the chunk is captured");
        events.note(&format!("captured {i}"));
    }
    let clock = FakeClock::new(NOW);
    let outbox_wake = Arc::new(Notify::new());
    let (acked, _) = watch::channel(0i64);
    let mut driver = Driver::spawn(Parts {
        store: store.clone(),
        stage: stage.clone(),
        stage2: Arc::new(Stage2::new(
            store.clone(),
            Arc::new(clock.clone()),
            outbox_wake.clone(),
            stage2::Timing::default(),
        )),
        clock: Arc::new(clock),
        wake,
        outbox_wake,
        merges: Arc::new(Notify::new()),
        acked: acked.subscribe(),
        timing: Timing::DEFAULT,
    });
    let core = Core {
        store: store.clone(),
        events,
        acked,
    };
    let deadline = tokio::time::Instant::now() + CHILD_LIMIT;
    loop {
        core.answer().await;
        if core.finished().await {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the push did not finish in {CHILD_LIMIT:?}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    tokio::time::timeout(Duration::from_secs(10), driver.stop())
        .await
        .expect("the driver ends when it is told");
    let _ = tokio::task::spawn_blocking(move || stage.close()).await;
    owner.close();
}

// ---------------------------------------------------------------------------------------------
// The parent.
// ---------------------------------------------------------------------------------------------

fn lines(dir: &Path) -> Vec<String> {
    kill_log::lines(&dir.join("events"))
}

/// The chunks whose capture had returned `Ok` (the child's lines).
fn captured(dir: &Path) -> BTreeSet<usize> {
    lines(dir)
        .iter()
        .filter_map(|l| l.strip_prefix("captured ")?.parse().ok())
        .collect()
}

fn reopen(dir: &Path) -> Connection {
    kourier_db::open(&dir.join("backend.db"), &DbKey::from(KEY)).expect("reopen with the key")
}

fn reopen_stage(dir: &Path) -> Connection {
    kourier_db::open(
        &dir.join(super::stage::DIR).join(super::stage::FILE),
        &derive_key(&DbKey::from(KEY)),
    )
    .expect("reopen the stage with its key")
}

fn count(conn: &Connection, sql_text: &str) -> i64 {
    conn.query_row(sql_text, [], |r| r.get(0)).expect("count")
}

/// Runs the child on `dir`; `point` arms a kill point (`name:n`). Returns how it ended and what it said.
fn run_child(dir: &Path, mode: &str, point: Option<&str>) -> (std::process::ExitStatus, String) {
    let mut cmd = Command::new(std::env::current_exe().expect("test binary"));
    cmd.args([
        "--exact",
        "history::kill::child",
        "--nocapture",
        "--test-threads=1",
    ])
    .env(MODE, mode)
    .env(DIR, dir)
    .env_remove("KOURIER_FAILPOINT")
    .env_remove(fault::ENV);
    if let Some(point) = point {
        cmd.env("KOURIER_FAILPOINT", point);
    }
    let out = cmd.output().expect("spawn the child");
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status, said)
}

/// Where a finished run stands: what the core was given, and what the books and the stage say.
#[derive(Debug, PartialEq, Eq)]
struct State {
    /// For each message the core was given (`conv | origin | local`) the outbox seqs it came under.
    given: BTreeMap<String, BTreeSet<i64>>,
    /// `(phase, conv, msg)` of the cursor and whether it stands on the last chunk of the stage.
    cursor: (i64, i64, i64, bool),
    /// `delivered, dropped, deferred, bad, lost` of `history_counts`.
    counts: [i64; 5],
    chunks_retired: i64,
    secrets: i64,
    kept: i64,
    /// `(notice id, state, bytes gone)` of the stage's chunks, by notice id.
    stage: Vec<(String, String, bool)>,
    outbox_left: i64,
}

fn state(dir: &Path) -> State {
    let mut given: BTreeMap<String, BTreeSet<i64>> = BTreeMap::new();
    for line in lines(dir) {
        let mut parts = line.split('\t');
        if parts.next() != Some("msg") {
            continue;
        }
        let seq: i64 = parts.next().and_then(|s| s.parse().ok()).expect("a seq");
        let identity: Vec<&str> = parts.collect();
        given.entry(identity.join(" | ")).or_default().insert(seq);
    }
    let conn = reopen(dir);
    let stage = reopen_stage(dir);
    let last: i64 = stage
        .query_row("SELECT max(seq) FROM chunks", [], |r| r.get(0))
        .expect("the last chunk");
    let (chunk, phase, conv, msg): (i64, i64, i64, i64) = conn
        .query_row(
            "SELECT chunk, phase, conv, msg FROM history_cursor WHERE id = 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .expect("the cursor");
    let counts: [i64; 5] = conn
        .query_row(
            "SELECT delivered, dropped, deferred, bad, lost FROM history_counts WHERE id = 1",
            [],
            |r| Ok([r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?]),
        )
        .expect("the counts");
    let mut stmt = stage
        .prepare("SELECT msg_id, state, NOT EXISTS (SELECT 1 FROM chunk_bytes b WHERE b.seq = chunks.seq) \
             FROM chunks ORDER BY msg_id")
        .expect("prepare");
    let stage_rows: Vec<(String, String, bool)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .expect("query")
        .map(|r| r.expect("row"))
        .collect();
    State {
        given,
        cursor: (phase, conv, msg, chunk == last),
        counts,
        chunks_retired: count(
            &conn,
            "SELECT count(*) FROM history_chunks WHERE retired = 1",
        ),
        secrets: count(&conn, "SELECT count(*) FROM msg_secrets"),
        kept: count(&conn, "SELECT count(*) FROM history_deferred"),
        stage: stage_rows,
        outbox_left: count(&conn, "SELECT count(*) FROM core_outbox"),
    }
}

/// The numbers the four chunks were made with, which the unkilled run must reach.
fn assert_made_with(state: &State) {
    let expected = expected_locals();
    let locals: BTreeSet<String> = state
        .given
        .keys()
        .map(|k| k.rsplit(" | ").next().expect("a local").to_owned())
        .collect();
    assert_eq!(locals, expected, "the core was given every message once");
    assert_eq!(
        state.given.len(),
        expected.len(),
        "no message appears under two identities"
    );
    assert_eq!(
        state.counts[0],
        i64::try_from(expected.len()).expect("small"),
        "history_counts.delivered counts the messages the core was given"
    );
    assert_eq!(state.kept, 1, "the poll vote is kept for a later build");
    assert_eq!(state.secrets, 3, "the three secrets are in the store");
    assert_eq!(state.chunks_retired, 4);
    assert!(state.cursor.3, "the cursor stands on the last chunk");
    assert_eq!(state.outbox_left, 0);
    let mut notices: Vec<String> = NOTICES.iter().map(|n| (*n).to_owned()).collect();
    notices.sort();
    assert_eq!(
        state.stage,
        notices
            .into_iter()
            .map(|n| (n, "done".to_owned(), true))
            .collect::<Vec<_>>(),
        "every chunk is done in the stage and its bytes are gone"
    );
}

/// What an unkilled run reaches (also held to the numbers the chunks were made with).
fn reference() -> State {
    let dir = tempfile::tempdir().expect("temp dir");
    let (status, said) = run_child(dir.path(), "first", None);
    assert!(status.success(), "the unkilled run failed: {said}");
    let state = state(dir.path());
    assert_made_with(&state);
    state
}

/// Kills the push at `point`, hit number `n`; the recovery then finishes it. `false` when the child ended
/// normally (the point is hit fewer than `n` times).
fn kill_once(point: &str, n: u32, reference: &State) -> bool {
    let dir = tempfile::tempdir().expect("temp dir");
    let what = format!("{point}:{n}");
    let (status, said) = run_child(dir.path(), "first", Some(&what));
    if status.success() {
        return false;
    }
    assert_eq!(
        status.signal(),
        Some(6),
        "{what} must abort: {status:?} {said}"
    );
    assert!(said.contains("hit: aborting"), "{what}: {said}");
    // A chunk whose capture returned Ok is in the stage: nothing was answered before it was durable.
    let told = captured(dir.path());
    if !told.is_empty() {
        let stage = reopen_stage(dir.path());
        for i in told {
            let rows = count(
                &stage,
                &format!(
                    "SELECT count(*) FROM chunks WHERE msg_id = '{}'",
                    NOTICES[i]
                ),
            );
            assert_eq!(
                rows, 1,
                "{what}: chunk {i} was captured, but it is not in the stage"
            );
        }
    }
    assert_done_follows_settle(dir.path(), &what);
    let (status, said) = run_child(dir.path(), "recover", None);
    assert!(status.success(), "{what}: the recovery failed: {said}");
    let after = state(dir.path());
    assert_eq!(
        after.given.keys().collect::<Vec<_>>(),
        reference.given.keys().collect::<Vec<_>>(),
        "{what}: the core was given the same messages as in the unkilled run"
    );
    for (message, seqs) in &after.given {
        assert_eq!(
            seqs.len(),
            1,
            "{what}: message {message} came under two outbox seqs: {seqs:?}"
        );
    }
    assert_eq!(
        (
            after.cursor,
            after.counts,
            after.chunks_retired,
            after.secrets,
            after.kept,
            &after.stage,
            after.outbox_left
        ),
        (
            reference.cursor,
            reference.counts,
            reference.chunks_retired,
            reference.secrets,
            reference.kept,
            &reference.stage,
            reference.outbox_left
        ),
        "{what}: the end state is the unkilled run's"
    );
    true
}

/// A chunk the stage calls `done` (its bytes gone, so it can never be read again) has been recorded as delivered and no
/// row of it waits: the stage's retiring job comes after the core answered, not before. Looked at the moment of the kill.
fn assert_done_follows_settle(dir: &Path, what: &str) {
    if !dir
        .join(super::stage::DIR)
        .join(super::stage::FILE)
        .exists()
    {
        return;
    }
    let stage = reopen_stage(dir);
    let mut statement = stage
        .prepare("SELECT seq FROM chunks WHERE state = 'done'")
        .expect("prepare");
    let done: Vec<i64> = statement
        .query_map([], |r| r.get(0))
        .expect("query")
        .map(|r| r.expect("row"))
        .collect();
    let books = reopen(dir);
    for seq in done {
        assert_eq!(
            count(
                &books,
                &format!("SELECT count(*) FROM history_chunks WHERE chunk = {seq}")
            ),
            1,
            "{what}: chunk {seq} is done in the stage, but the books do not record it as delivered"
        );
        assert_eq!(
            count(
                &books,
                &format!("SELECT count(*) FROM history_delivery WHERE chunk = {seq}")
            ),
            0,
            "{what}: chunk {seq} is done in the stage, but a row of it still waits for the core"
        );
    }
}

/// The eight points of the history, with the hit the sample kills at (a hit EVERY run makes, whatever the timing): the
/// captures and the rows are as many as the chunks make; a settle runs before the third row is appended (the window
/// is two) and again after the last; but ONE retire job takes every chunk delivered by then, and the driver looks only
/// after a wake or when the pump is quiet, so how many jobs a run makes depends on how the core's answers fall between
/// the pump's steps: only the first is certain (strike of 2026-10-27: `wa_inside_history_done:2` was never hit on a
/// loaded build server, where all four chunks were retired by one job; the loop below kills at every hit there is).
const POINTS: [(&str, u32); 8] = [
    (fp::INSIDE_HISTORY_CAPTURE, 2),
    (fp::AFTER_HISTORY_CAPTURE, 3),
    (fp::INSIDE_HISTORY_ROW, 3),
    (fp::AFTER_HISTORY_ROW, 4),
    (fp::INSIDE_HISTORY_SETTLE, 2),
    (fp::AFTER_HISTORY_SETTLE, 2),
    (fp::INSIDE_HISTORY_DONE, 1),
    (fp::AFTER_HISTORY_DONE, 1),
];

/// One kill at each point: the regular set's sample of the loop below.
#[test]
fn a_kill_at_each_point_loses_no_captured_message_and_doubles_none() {
    let reference = reference();
    for (point, n) in POINTS {
        assert!(
            kill_once(point, n, &reference),
            "{point}:{n} was never hit: the sample saw nothing"
        );
    }
}

/// Kills the push at `point:1`, `point:2`, ... until the child ends normally; returns how many kills there were.
fn kill_at_every_hit(point: &str, reference: &State) -> u32 {
    let mut kills = 0;
    for n in 1..=MAX_HITS + 1 {
        assert!(n <= MAX_HITS, "{point} is hit more than {MAX_HITS} times");
        if !kill_once(point, n, reference) {
            break;
        }
        kills += 1;
    }
    assert!(
        kills >= 1,
        "{point} was never hit: the kill test saw nothing"
    );
    kills
}

#[test]
fn every_captured_message_once_after_a_kill_at_every_point() {
    let reference = reference();
    let hits: Vec<(&str, u32)> = POINTS
        .iter()
        .map(|(point, _)| (*point, kill_at_every_hit(point, &reference)))
        .collect();
    eprintln!("kills per point: {hits:?}");
}
