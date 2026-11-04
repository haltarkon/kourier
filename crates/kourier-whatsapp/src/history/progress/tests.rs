//! Progress and the end of a push (W4 C15): what the batches of each sync type say, the order the chunks are
//! delivered in, and when a chat gets its end. The first tests are the rules alone ([`Facts`]); the rest run
//! the real feeder and the pump over a real stage and `backend.db`, with a core that takes every row
//! (`drive`) and a record of every event it was given, in order. Every id, number and name is synthetic.

use std::collections::BTreeSet;
use std::sync::Arc;

use kourier_backend_api::{BackendEvent, FakeClock, decode_outbox_row};
use kourier_db::{DbKey, Priority, outbox};
use kourier_model::UserId;
use wacore_binary::Jid;

use super::{Delivered, Facts, PUSH_QUIET_MS, Staged, State, fraction, pushes_chats, rank};
use crate::error::sql;
use crate::history::deliver::Own;
use crate::history::feeder::Feeder;
use crate::history::pump::{Pump, Rows, Step};
use crate::history::rows::{Body, Cursor, Phase};
use crate::history::settle::Limits;
use crate::history::stage::{MIGRATIONS, StageHandle, StageOwner, derive_key};
use crate::history::testing::{Chat, Chunk, Msg, SyncType};
use crate::map::{Ctx, OwnIds};
use crate::store::{KourierStore, StoreOwner};

const P1: &str = "15550100101@s.whatsapp.net";
const P2: &str = "15550100102@s.whatsapp.net";
const P3: &str = "15550100103@s.whatsapp.net";
const OWN_PN: &str = "15550100999";
const OWN_LID: &str = "99900100999";
const NOW: u64 = 1_700_000_500_000;

const BOOTSTRAP: Option<i32> = Some(0);
const RECENT: Option<i32> = Some(3);

// ---- the rules alone ---------------------------------------------------------------------------------

fn staged(seq: i64, id: &str, sync_type: Option<i32>, order: u32, state: State) -> Staged {
    Staged {
        seq,
        msg_id: id.to_owned(),
        sync_type,
        order: Some(order),
        state,
        captured_at: NOW as i64,
    }
}

fn delivered(chunk: i64, sync_type: Option<i32>, order: u32, progress: u32) -> Delivered {
    Delivered {
        chunk,
        sync_type,
        order: Some(order),
        progress: Some(progress),
    }
}

#[test]
fn only_the_sync_types_that_push_chats_say_has_more() {
    for pushes in [Some(0), Some(2), Some(3), None, Some(77)] {
        assert!(pushes_chats(pushes), "{pushes:?}");
    }
    // Status updates, names, the data that blocks nothing, and the answer to a request.
    for quiet in [Some(1), Some(4), Some(5), Some(6)] {
        assert!(!pushes_chats(quiet), "{quiet:?}");
    }
    assert_eq!(fraction(Some(40)), Some(0.4));
    assert_eq!(
        fraction(Some(250)),
        Some(1.0),
        "the phone's number is bounded"
    );
    assert_eq!(fraction(None), None);
}

#[test]
fn a_push_is_over_at_its_last_chunk_and_not_before() {
    let mut facts = Facts {
        staged: vec![staged(1, "n1", BOOTSTRAP, 1, State::Planned)],
        delivered: vec![delivered(1, BOOTSTRAP, 1, 60)],
        announced: vec!["n1".to_owned()],
        ..Facts::default()
    };
    assert!(!facts.over(BOOTSTRAP), "60 % is not the last chunk");
    facts
        .staged
        .push(staged(2, "n2", BOOTSTRAP, 2, State::Planned));
    facts.announced.push("n2".to_owned());
    facts.delivered.push(delivered(2, BOOTSTRAP, 2, 100));
    assert!(facts.over(BOOTSTRAP));
    assert_eq!(facts.over_types(), vec![BOOTSTRAP]);
    assert!(!facts.over(RECENT), "another type has no last chunk");
}

#[test]
fn chunk_3_at_100_before_chunk_2_ends_nothing_early_by_the_rules() {
    // Chunk 3 (100 %) is delivered; chunk 2 is in the stage and waits.
    let mut facts = Facts {
        staged: vec![
            staged(1, "n3", BOOTSTRAP, 3, State::Planned),
            staged(2, "n2", BOOTSTRAP, 2, State::Planned),
        ],
        delivered: vec![delivered(1, BOOTSTRAP, 3, 100)],
        announced: vec!["n3".to_owned(), "n2".to_owned()],
        ..Facts::default()
    };
    assert!(!facts.over(BOOTSTRAP), "a lower chunk waits in the stage");
    // It is delivered: nothing waits any more.
    facts.delivered.push(delivered(2, BOOTSTRAP, 2, 60));
    assert!(facts.over(BOOTSTRAP));

    // The same with chunk 2 only announced: its notice is open until its chunk is in the stage.
    let facts = Facts {
        staged: vec![staged(1, "n3", BOOTSTRAP, 3, State::Planned)],
        delivered: vec![delivered(1, BOOTSTRAP, 3, 100)],
        announced: vec!["n3".to_owned(), "n2".to_owned()],
        ..Facts::default()
    };
    assert_eq!(facts.unresolved(), 1);
    assert!(
        !facts.over(BOOTSTRAP),
        "an announced chunk may still arrive"
    );
    // A notice counted lost is no longer in the list (`history_announced.lost`).
    let facts = Facts {
        announced: vec!["n3".to_owned()],
        ..facts
    };
    assert!(facts.over(BOOTSTRAP));
}

#[test]
fn what_waits_in_the_stage_holds_back_only_its_own_type_and_the_unknown() {
    let delivered_100 = vec![delivered(1, BOOTSTRAP, 1, 100)];
    let base = |waiting: Staged| Facts {
        staged: vec![staged(1, "n1", BOOTSTRAP, 1, State::Planned), waiting],
        delivered: delivered_100.clone(),
        announced: vec!["n1".to_owned(), "w".to_owned()],
        ..Facts::default()
    };
    assert!(!base(staged(2, "w", BOOTSTRAP, 9, State::Captured)).over(BOOTSTRAP));
    assert!(base(staged(2, "w", RECENT, 1, State::Captured)).over(BOOTSTRAP));
    assert!(
        !base(staged(2, "w", None, 1, State::Captured)).over(BOOTSTRAP),
        "a chunk of no known type could be of this one"
    );
    // A chunk set aside (bad) or done is no chunk that can still be delivered.
    assert!(base(staged(2, "w", BOOTSTRAP, 9, State::Bad)).over(BOOTSTRAP));
    assert!(base(staged(2, "w", BOOTSTRAP, 9, State::Done)).over(BOOTSTRAP));
}

#[test]
fn a_chunk_is_owed_its_record_while_it_is_read_and_not_delivered() {
    let mut facts = Facts {
        staged: vec![
            staged(1, "a", BOOTSTRAP, 1, State::Planned),
            staged(2, "b", BOOTSTRAP, 2, State::Bad),
        ],
        ..Facts::default()
    };
    assert!(facts.owes_record(1));
    assert!(
        !facts.owes_record(2),
        "a chunk set aside is not delivered ever"
    );
    assert!(
        !facts.owes_record(9),
        "a chunk the stage does not hold has no record to make"
    );
    facts.delivered.push(delivered(1, BOOTSTRAP, 1, 40));
    assert!(!facts.owes_record(1));
}

#[test]
fn chunks_are_chosen_by_type_then_order_then_capture() {
    let facts = Facts {
        staged: vec![
            staged(1, "a", RECENT, 1, State::Planned),
            staged(2, "b", BOOTSTRAP, 3, State::Planned),
            staged(3, "c", BOOTSTRAP, 2, State::Planned),
            staged(4, "d", Some(4), 9, State::Planned),
            staged(5, "e", BOOTSTRAP, 1, State::Captured),
            staged(6, "f", BOOTSTRAP, 1, State::Bad),
        ],
        delivered: vec![],
        announced: vec![],
        ..Facts::default()
    };
    // The names first; then the bootstrap by its order (a chunk not read yet and a bad one are not chosen).
    assert_eq!(facts.next_chunk(), Some(4));
    let mut facts = facts;
    facts.delivered.push(delivered(4, Some(4), 9, 0));
    assert_eq!(facts.next_chunk(), Some(3));
    facts.delivered.push(delivered(3, BOOTSTRAP, 2, 40));
    assert_eq!(facts.next_chunk(), Some(2));
    facts.delivered.push(delivered(2, BOOTSTRAP, 3, 100));
    assert_eq!(facts.next_chunk(), Some(1), "the long history comes last");
    assert!(rank(Some(4)) < rank(BOOTSTRAP) && rank(BOOTSTRAP) < rank(RECENT));
}

/// F6: a push whose last chunk is lost or does not read still ends, once nothing can arrive (no chunk waits, no
/// notice is open) and the push cannot be taken for running (a chunk was lost or bad, or the newest capture is old).
/// Mutant for the re-check: `account_over` answering false.
#[test]
fn a_push_whose_last_chunk_is_lost_or_bad_ends_when_nothing_can_arrive() {
    let one = || Facts {
        staged: vec![staged(1, "n1", RECENT, 1, State::Planned)],
        delivered: vec![delivered(1, RECENT, 1, 50)],
        announced: vec!["n1".to_owned()],
        now_ms: NOW as i64,
        ..Facts::default()
    };
    // The 100 % chunk never came, nothing is wrong that we know of, and the capture is fresh: still running.
    assert!(!one().over(RECENT));
    assert!(
        !one().account_over(),
        "a fresh capture: the phone may go on"
    );
    // The last capture is older than the quiet time: over.
    let late = Facts {
        now_ms: NOW as i64 + PUSH_QUIET_MS,
        ..one()
    };
    assert!(late.account_over());
    // A chunk that did not read, or one counted lost, ends it at once.
    let mut bad = one();
    bad.staged.push(staged(2, "n2", RECENT, 2, State::Bad));
    bad.announced.push("n2".to_owned());
    assert!(bad.account_over());
    let lost = Facts {
        lost: true,
        ..one()
    };
    assert!(lost.account_over());
    // But not while a chunk waits or a notice is open.
    let mut waiting = bad.clone();
    waiting
        .staged
        .push(staged(3, "n3", RECENT, 3, State::Captured));
    assert!(!waiting.account_over());
    let open = Facts {
        announced: vec!["n1".to_owned(), "n9".to_owned()],
        lost: true,
        ..one()
    };
    assert!(!open.account_over(), "an announced chunk may still arrive");
    assert!(
        !Facts::default().account_over(),
        "nothing was ever captured"
    );
}

// ---- the real feeder and the pump --------------------------------------------------------------------

struct World {
    _dir: tempfile::TempDir,
    _owner: StoreOwner,
    stage_owner: StageOwner,
    store: KourierStore,
    clock: FakeClock,
}

fn world() -> World {
    let dir = tempfile::tempdir().expect("temp dir");
    let owner = StoreOwner::open(
        &dir.path().join("backend.db"),
        &DbKey::from([9; 32]),
        "wa-progress",
    )
    .expect("open");
    let store = owner.store();
    let stage_owner = StageOwner::open(
        &dir.path().join("history").join("stage.db"),
        &derive_key(&DbKey::from([0x22; 32])),
        "the account",
        &MIGRATIONS,
    )
    .expect("open the stage")
    .0;
    World {
        _dir: dir,
        _owner: owner,
        stage_owner,
        store,
        clock: FakeClock::new(NOW),
    }
}

fn ctx() -> Ctx {
    Ctx {
        own: OwnIds {
            lid: Some(UserId::from_parts("lid", OWN_LID).expect("lid")),
            pn: Some(UserId::from_parts("pn", OWN_PN).expect("pn")),
        },
    }
}

fn own() -> Own {
    Own {
        pn: Some(Jid::pn(OWN_PN)),
        lid: Some(Jid::lid(OWN_LID)),
    }
}

impl World {
    fn stage(&self) -> StageHandle {
        self.stage_owner.handle()
    }

    /// Captures `chunk` as the capture hook does: the bytes and the sync type; its order and progress are
    /// read from the blob when it is planned.
    async fn put(&self, id: &str, chunk: &Chunk) -> i64 {
        let (id, bytes, sync) = (id.to_owned(), chunk.blob(), chunk.sync.sync_type as i32);
        self.stage()
            .write_at(Priority::High, "test capture", None, move |tx| {
                tx.execute(
                    "INSERT INTO chunks (msg_id, sync_type, captured_at, size, state) \
                     VALUES (?1, ?2, ?4, ?3, 'captured')",
                    (id, sync, bytes.len() as i64, NOW as i64),
                )
                .map_err(sql("test capture"))?;
                let seq = tx.last_insert_rowid();
                tx.execute(
                    "INSERT INTO chunk_bytes (seq, bytes) VALUES (?1, ?2)",
                    (seq, bytes),
                )
                .map_err(sql("test capture"))?;
                Ok(seq)
            })
            .await
            .expect("capture")
    }

    /// The notice of a chunk is recorded (its chunk is not captured).
    async fn announce(&self, id: &str) {
        let id = id.to_owned();
        self.store
            .write("test announce", move |tx| {
                tx.execute(
                    "INSERT INTO history_announced(msg_id, announced_at) VALUES (?1, 1)",
                    [id],
                )
                .map(drop)
                .map_err(sql("announce"))
            })
            .await
            .expect("announce");
    }

    async fn pump(&self) -> Pump<Feeder> {
        let feeder = Feeder::new(
            self.stage(),
            self.store.clone(),
            ctx(),
            Arc::new(self.clock.clone()),
        );
        Pump::start(
            self.store.clone(),
            feeder,
            own(),
            Arc::new(self.clock.clone()),
            Limits::DEFAULT,
        )
        .await
        .expect("the pump starts")
    }

    async fn count(&self, what: &'static str) -> i64 {
        self.store
            .read(move |conn| {
                conn.query_row(&format!("SELECT count(*) FROM {what}"), [], |r| r.get(0))
                    .map_err(sql("count"))
            })
            .await
            .expect("count")
    }
}

/// A chat of the given messages ("A1".. in the order given, a minute apart) and last activity.
fn chat(jid: &str, active: u64, ids: &[&str]) -> Chat {
    ids.iter()
        .zip(0u64..)
        .fold(Chat::new(jid).active_at(active), |chat, (id, n)| {
            chat.message(Msg::text(jid, id, 1_700_000_100 + 60 * n, id))
        })
}

/// The core takes every row there is, in order, and keeps what it was given.
async fn ack_all(w: &World, seen: &mut Vec<BackendEvent>) {
    let rows = w
        .store
        .read(|conn| outbox::peek(conn, 1000).map_err(|_| crate::error::database("peek")))
        .await
        .expect("peek");
    for row in rows {
        let events = decode_outbox_row(&row.envelope).expect("decodes");
        let seq = row.seq;
        w.store
            .write("test core", move |tx| {
                tx.execute("DELETE FROM core_outbox WHERE seq = ?1", [seq])
                    .map(drop)
                    .map_err(sql("ack"))
            })
            .await
            .expect("ack");
        seen.extend(events);
    }
}

/// Runs the pump until it has nothing to do, the core taking every row.
async fn drive(w: &World, pump: &mut Pump<Feeder>, seen: &mut Vec<BackendEvent>) {
    for _ in 0..600 {
        match pump.step().await.expect("a step") {
            Step::Progress { .. } => {}
            Step::Wait { .. } => ack_all(w, seen).await,
            Step::Quiet => {
                ack_all(w, seen).await;
                return;
            }
        }
    }
    panic!("the pump did not come to rest");
}

/// One batch as the core sees it: (conversation, messages, has_more, progress).
type Batch = (String, usize, bool, Option<f32>);

fn batches(events: &[BackendEvent]) -> Vec<Batch> {
    events
        .iter()
        .filter_map(|event| match event {
            BackendEvent::HistoryBatch {
                conv,
                messages,
                has_more,
                progress,
            } => Some((conv.encode(), messages.len(), *has_more, *progress)),
            _ => None,
        })
        .collect()
}

/// The conversations the batches touched.
fn touched(all: &[Batch]) -> BTreeSet<String> {
    all.iter().map(|b| b.0.clone()).collect()
}

/// The ends stated so far: the empty batches that say `has_more: false`.
fn ends(all: &[Batch]) -> Vec<&Batch> {
    all.iter().filter(|b| b.1 == 0 && !b.2).collect()
}

/// Every Rust file below `dir` that is not a test file (`tests.rs`, `tests/`).
fn sources(dir: &std::path::Path, found: &mut Vec<(std::path::PathBuf, String)>) {
    for entry in std::fs::read_dir(dir).expect("read the source directory") {
        let path = entry.expect("an entry").path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if path.is_dir() {
            if name != "tests" {
                sources(&path, found);
            }
        } else if name.ends_with(".rs") && name != "tests.rs" {
            let text = std::fs::read_to_string(&path).expect("read a source file");
            found.push((path, text));
        }
    }
}

// ---- the tests ---------------------------------------------------------------------------------------

#[tokio::test]
async fn the_push_ends_with_has_more_false_for_every_chat_it_touched() {
    let w = world();
    w.put(
        "n1",
        &Chunk::new(SyncType::INITIAL_BOOTSTRAP, 1, 50)
            .chat(chat(P1, 1_700_000_400, &["A1", "A2"]))
            .chat(chat(P2, 1_700_000_300, &["B1"])),
    )
    .await;
    w.put(
        "n2",
        &Chunk::new(SyncType::INITIAL_BOOTSTRAP, 2, 100)
            .chat(chat(P2, 1_700_000_500, &["B2"]))
            .chat(chat(P3, 1_700_000_200, &["C1"])),
    )
    .await;
    let mut pump = w.pump().await;
    let mut seen = Vec::new();
    drive(&w, &mut pump, &mut seen).await;

    let all = batches(&seen);
    let convs = touched(&all);
    assert_eq!(convs.len(), 3, "{all:?}");
    for conv in &convs {
        let of: Vec<&Batch> = all.iter().filter(|b| &b.0 == conv).collect();
        let (last, earlier) = of.split_last().expect("a batch");
        assert!(
            last.1 == 0 && !last.2 && last.3.is_none(),
            "the last batch of a chat ends it: {of:?}"
        );
        assert!(
            earlier.iter().all(|b| b.2),
            "every batch before the end says more follows: {of:?}"
        );
    }
    assert_eq!(ends(&all).len(), 3, "each chat is ended once: {all:?}");
    assert_eq!(w.count("history_chats WHERE more = 1").await, 0);

    // Every batch with messages carries its own chunk's progress.
    let progress: Vec<Option<f32>> = all.iter().filter(|b| b.1 > 0).map(|b| b.3).collect();
    assert_eq!(progress.iter().filter(|p| **p == Some(0.5)).count(), 2);
    assert_eq!(progress.iter().filter(|p| **p == Some(1.0)).count(), 2);
    assert_eq!(progress.len(), 4);

    // A second look finds nothing owed.
    let before = seen.len();
    drive(&w, &mut pump, &mut seen).await;
    assert_eq!(seen.len(), before, "the push ends once");
}

#[tokio::test]
async fn chunk_3_at_100_before_chunk_2_ends_nothing_early() {
    let w = world();
    // The phone announced chunks 2 and 3; chunk 3 (100 %) is captured first.
    w.announce("n3").await;
    w.announce("n2").await;
    w.put(
        "n3",
        &Chunk::new(SyncType::INITIAL_BOOTSTRAP, 3, 100).chat(chat(P1, 1_700_000_400, &["A1"])),
    )
    .await;
    let mut pump = w.pump().await;
    let mut seen = Vec::new();
    drive(&w, &mut pump, &mut seen).await;
    let all = batches(&seen);
    assert_eq!(all.len(), 1, "{all:?}");
    assert!(
        all[0].2 && all[0].1 == 1,
        "the chat still has more: {all:?}"
    );
    assert!(ends(&all).is_empty(), "nothing ended early: {all:?}");
    assert_eq!(w.count("history_chats WHERE more = 1").await, 1);

    // Chunk 2 arrives and is delivered: now the push is over, for both chats.
    w.put(
        "n2",
        &Chunk::new(SyncType::INITIAL_BOOTSTRAP, 2, 60).chat(chat(P2, 1_700_000_300, &["B1"])),
    )
    .await;
    drive(&w, &mut pump, &mut seen).await;
    let all = batches(&seen);
    assert_eq!(ends(&all).len(), 2, "{all:?}");
    let last_message = all.iter().rposition(|b| b.1 > 0).expect("a message batch");
    let first_end = all.iter().position(|b| b.1 == 0 && !b.2).expect("an end");
    assert!(
        last_message < first_end,
        "the ends come behind the last chunk: {all:?}"
    );
    assert_eq!(w.count("history_chats WHERE more = 1").await, 0);
}

#[tokio::test]
async fn a_chunk_that_is_in_the_stage_when_chunk_3_is_chosen_is_delivered_first() {
    let w = world();
    w.put(
        "n3",
        &Chunk::new(SyncType::INITIAL_BOOTSTRAP, 3, 100).chat(chat(P3, 1_700_000_200, &["C1"])),
    )
    .await;
    w.put(
        "n2",
        &Chunk::new(SyncType::INITIAL_BOOTSTRAP, 2, 60).chat(chat(P2, 1_700_000_300, &["B1"])),
    )
    .await;
    w.put(
        "n1",
        &Chunk::new(SyncType::INITIAL_BOOTSTRAP, 1, 30).chat(chat(P1, 1_700_000_400, &["A1"])),
    )
    .await;
    let mut pump = w.pump().await;
    let mut seen = Vec::new();
    drive(&w, &mut pump, &mut seen).await;
    let all = batches(&seen);
    let order: Vec<&String> = all.iter().filter(|b| b.1 > 0).map(|b| &b.0).collect();
    assert_eq!(order.len(), 3);
    let progress: Vec<Option<f32>> = all.iter().filter(|b| b.1 > 0).map(|b| b.3).collect();
    assert_eq!(
        progress,
        vec![Some(0.3), Some(0.6), Some(1.0)],
        "by chunk order, whatever the order of capture"
    );
    assert_eq!(ends(&all).len(), 3, "{all:?}");
}

#[tokio::test]
async fn a_chat_the_phone_says_it_has_no_more_of_ends_with_its_chunk() {
    let w = world();
    // Chunk 1 is not the last (30 %): the push goes on. The phone says P1 is complete.
    w.put(
        "n1",
        &Chunk::new(SyncType::INITIAL_BOOTSTRAP, 1, 30)
            .chat(chat(P1, 1_700_000_400, &["A1"]).end_of_history())
            .chat(chat(P2, 1_700_000_300, &["B1"])),
    )
    .await;
    w.put(
        "n2",
        &Chunk::new(SyncType::INITIAL_BOOTSTRAP, 2, 60).chat(chat(P3, 1_700_000_200, &["C1"])),
    )
    .await;
    let mut pump = w.pump().await;
    let mut seen = Vec::new();
    drive(&w, &mut pump, &mut seen).await;
    let all = batches(&seen);
    let conv_of = |jid: &str| -> String {
        all.iter()
            .find(|b| b.1 == 1 && b.0.contains(&jid[..11]))
            .unwrap_or_else(|| panic!("a batch of the chat: {all:?}"))
            .0
            .clone()
    };
    let (p1, p2, p3) = (conv_of(P1), conv_of(P2), conv_of(P3));
    let end_of = |conv: &String| all.iter().position(|b| &b.0 == conv && b.1 == 0 && !b.2);
    let first_of_chunk_2 = all.iter().position(|b| b.0 == p3).expect("chunk 2");
    assert!(
        end_of(&p1).is_some_and(|at| at < first_of_chunk_2),
        "P1 ends with its chunk, before the next one: {all:?}"
    );
    assert!(end_of(&p2).is_none(), "the push is not over: {all:?}");
    assert!(end_of(&p3).is_none(), "the push is not over: {all:?}");
    assert_eq!(w.count("history_chats WHERE more = 1").await, 2);
}

#[tokio::test]
async fn chunks_that_are_no_push_never_say_has_more() {
    let w = world();
    w.put(
        "n1",
        &Chunk::new(SyncType::PUSH_NAME, 1, 0)
            .chat(chat(P1, 1_700_000_400, &["A1"]))
            .push_name(P1, "A Name"),
    )
    .await;
    w.put(
        "n2",
        &Chunk::new(SyncType::NON_BLOCKING_DATA, 1, 100).chat(chat(P2, 1_700_000_300, &["B1"])),
    )
    .await;
    w.put(
        "n3",
        &Chunk::new(SyncType::ON_DEMAND, 1, 100).chat(chat(P3, 1_700_000_200, &["C1"])),
    )
    .await;
    let mut pump = w.pump().await;
    let mut seen = Vec::new();
    drive(&w, &mut pump, &mut seen).await;
    let all = batches(&seen);
    assert_eq!(all.len(), 3, "{all:?}");
    for batch in &all {
        assert!(!batch.2, "no more follows: {batch:?}");
        assert!(
            batch.3.is_none(),
            "no part of the push's progress: {batch:?}"
        );
    }
    assert_eq!(w.count("history_chats WHERE more = 1").await, 0);
    assert!(ends(&all).is_empty(), "nothing is owed an end: {all:?}");
}

#[tokio::test]
async fn no_account_progress_is_ever_sent() {
    let w = world();
    w.put(
        "n1",
        &Chunk::new(SyncType::INITIAL_BOOTSTRAP, 1, 50)
            .chat(chat(P1, 1_700_000_400, &["A1", "A2"]))
            .push_name(P1, "A Name"),
    )
    .await;
    w.put(
        "n2",
        &Chunk::new(SyncType::INITIAL_BOOTSTRAP, 2, 100).chat(chat(P2, 1_700_000_300, &["B1"])),
    )
    .await;
    let mut pump = w.pump().await;
    let mut seen = Vec::new();
    drive(&w, &mut pump, &mut seen).await;
    assert!(!seen.is_empty());
    // The viewmodel prefers an account's progress to the fill banner and would read "Updating contacts".
    // An `AccountProgress` is ephemeral and never rides in `core_outbox`, so the rows prove little: the
    // source of the crate must not name it at all (a test file may, to say so).
    let mut named = Vec::new();
    sources(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut named,
    );
    assert!(!named.is_empty(), "the scan found no source");
    let needle = ["Account", "Progress"].concat();
    for (path, text) in &named {
        assert!(
            !text.contains(&needle),
            "{} names the account's progress: the history never sends it",
            path.display()
        );
    }
    for event in &seen {
        assert!(
            matches!(
                event,
                BackendEvent::HistoryBatch { .. }
                    | BackendEvent::ConversationUpsert(_)
                    | BackendEvent::ContactUpsert(_)
                    | BackendEvent::IdentityMerged { .. }
            ),
            "{}",
            event.name()
        );
    }
}

#[tokio::test]
async fn a_chunk_the_stage_no_longer_holds_is_given_up_and_the_next_goes_on() {
    let w = world();
    // The cursor stands inside chunk 7, which the stage does not have (a stage made anew).
    w.store
        .write("test cursor", |tx| {
            tx.execute(
                "UPDATE history_cursor SET chunk = 7, phase = 2, conv = 1, msg = 3 WHERE id = 1",
                [],
            )
            .map(drop)
            .map_err(sql("cursor"))
        })
        .await
        .expect("cursor");
    w.put(
        "n1",
        &Chunk::new(SyncType::INITIAL_BOOTSTRAP, 1, 100).chat(chat(P1, 1_700_000_400, &["A1"])),
    )
    .await;
    let mut pump = w.pump().await;
    let mut seen = Vec::new();
    drive(&w, &mut pump, &mut seen).await;
    let all = batches(&seen);
    assert_eq!(all.len(), 2, "the message and the end: {all:?}");
    // The lost chunk is no delivered one, and the new one is.
    assert_eq!(w.count("history_chunks WHERE chunk = 7").await, 0);
    assert_eq!(w.count("history_chunks WHERE chunk = 1").await, 1);
}

#[tokio::test]
async fn the_feeder_gives_the_same_row_for_the_same_cursor() {
    let w = world();
    w.put(
        "n1",
        &Chunk::new(SyncType::INITIAL_BOOTSTRAP, 1, 100).chat(chat(P1, 1_700_000_400, &["A1"])),
    )
    .await;
    let mut feeder = Feeder::new(w.stage(), w.store.clone(), ctx(), Arc::new(w.clock.clone()));
    let first = feeder.next(Cursor::at(0, Phase::People, 0, 0)).await;
    let again = feeder.next(Cursor::at(0, Phase::People, 0, 0)).await;
    let (a, b) = (
        first.expect("row").expect("some"),
        again.expect("row").expect("some"),
    );
    assert_eq!(a.1.before, b.1.before);
    assert_eq!(a.1.after, b.1.after);
    assert!(matches!(a.1.body, Body::People { .. } | Body::Chats(_)));
    assert_eq!(a.0.msg_id, b.0.msg_id);
}

/// F6, the reviewer's case: the 100 % chunk of a sync type does not read, so no delivered chunk says 100; the chats
/// of the chunks before it still get their end once nothing can arrive.
/// Mutant for the re-check: `ending_row` without `account_over`.
#[tokio::test]
async fn a_sync_type_whose_last_chunk_does_not_read_still_ends_its_chats() {
    let w = world();
    w.put(
        "n1",
        &Chunk::new(SyncType::RECENT, 1, 50).chat(chat(P1, 1_700_000_400, &["A1", "A2"])),
    )
    .await;
    // The last chunk of the type: bytes that are no zlib stream.
    let bad = w.stage();
    bad.write_at(Priority::High, "test capture", None, |tx| {
        tx.execute(
            "INSERT INTO chunks (msg_id, sync_type, captured_at, size, state) \
             VALUES ('n2', 3, ?1, 4, 'captured')",
            [NOW as i64],
        )
        .map_err(sql("test capture"))?;
        tx.execute(
            "INSERT INTO chunk_bytes (seq, bytes) VALUES (?1, x'01020304')",
            [tx.last_insert_rowid()],
        )
        .map_err(sql("test capture"))?;
        Ok(())
    })
    .await
    .expect("capture the bad chunk");
    w.announce("n1").await;
    w.announce("n2").await;
    let mut pump = w.pump().await;
    let mut seen = Vec::new();
    drive(&w, &mut pump, &mut seen).await;
    // The first look planned the bad chunk and delivered chunk 1; the end follows at the next look.
    drive(&w, &mut pump, &mut seen).await;
    let all = batches(&seen);
    assert_eq!(ends(&all).len(), 1, "the chat got its end: {all:?}");
    assert_eq!(w.count("history_chats WHERE more = 1").await, 0);
}

/// F6: a lost last chunk (its notice counted lost) ends the chats too; an OPEN notice does not; and a quiet push ends
/// when the newest capture is six hours old, not before.
/// Mutant for the re-check: the `lost` or the quiet-time arm of `account_over` removed.
#[tokio::test]
async fn a_lost_last_chunk_and_a_quiet_push_end_the_chats_and_an_open_notice_does_not() {
    let w = world();
    w.put(
        "n1",
        &Chunk::new(SyncType::RECENT, 1, 50).chat(chat(P1, 1_700_000_400, &["A1"])),
    )
    .await;
    w.announce("n1").await;
    w.announce("n2").await;
    let mut pump = w.pump().await;
    let mut seen = Vec::new();
    drive(&w, &mut pump, &mut seen).await;
    drive(&w, &mut pump, &mut seen).await;
    assert_eq!(
        ends(&batches(&seen)).len(),
        0,
        "the notice of n2 is open: its chunk may still arrive"
    );
    w.store
        .write("test lost", |tx| {
            tx.execute_batch(
                "UPDATE history_announced SET lost = 1 WHERE msg_id = 'n2'; \
                 UPDATE history_counts SET lost = 1;",
            )
            .map_err(sql("test lost"))
        })
        .await
        .expect("n2 is counted lost");
    drive(&w, &mut pump, &mut seen).await;
    assert_eq!(
        ends(&batches(&seen)).len(),
        1,
        "n2 was lost: nothing can still arrive"
    );

    // The same push without any loss: it ends when the newest capture is six hours old.
    let w = world();
    w.put(
        "n1",
        &Chunk::new(SyncType::RECENT, 1, 50).chat(chat(P2, 1_700_000_400, &["B1"])),
    )
    .await;
    let mut pump = w.pump().await;
    let mut seen = Vec::new();
    drive(&w, &mut pump, &mut seen).await;
    drive(&w, &mut pump, &mut seen).await;
    assert_eq!(ends(&batches(&seen)).len(), 0, "a fresh push goes on");
    w.clock.advance(std::time::Duration::from_millis(
        u64::try_from(PUSH_QUIET_MS).expect("positive"),
    ));
    drive(&w, &mut pump, &mut seen).await;
    assert_eq!(ends(&batches(&seen)).len(), 1, "six quiet hours end it");
}
