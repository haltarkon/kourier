//! The driver over a real store and stage, no client (W4 C16): a captured chunk reaches the core with nothing but
//! the capture's wake, nothing is appended while the link hold is closed, the driver ends when it is told, a
//! replaced stage starts the books over, and a delivered chunk is retired. The core is a few lines (`ack_all`): it
//! takes the rows of `core_outbox` and answers them. Every id, number and secret is synthetic.

use std::sync::Arc;
use std::time::Duration;

use kourier_backend_api::{BackendEvent, FakeClock, decode_outbox_row};
use kourier_db::{DbKey, Priority, outbox};
use tokio::sync::{Notify, watch};
use wacore::store::device::Device;
use wacore::store::traits::DeviceStore as _;
use waproto::buffa::Message as _;
use waproto::whatsapp as wa;

use super::{Driver, Parts, Timing};
use crate::error::sql;
use crate::history::capture::Capture;
use crate::history::stage::{LazyStage, MIGRATIONS, StageOwner, derive_key};
use crate::history::testing::{Chat, Chunk, Msg, SyncType};
use crate::stage2::{self, Stage2};
use crate::store::{KourierStore, StoreOwner};

const KEY: [u8; 32] = [0x44; 32];
const PN: &str = "15550100123";
const LID: &str = "99900100123";
const PEER: &str = "15550100101@s.whatsapp.net";
const NOW: u64 = 1_800_000_000_000;

struct Rig {
    dir: tempfile::TempDir,
    _owner: StoreOwner,
    store: KourierStore,
    stage: Arc<LazyStage>,
    capture: Capture,
    wake: Arc<Notify>,
    outbox_wake: Arc<Notify>,
    /// The merge sweep's wake the driver was given (W6a2, B9).
    merges: Arc<Notify>,
    acked: watch::Sender<i64>,
    clock: FakeClock,
    stage2: Arc<Stage2>,
}

async fn rig() -> Rig {
    let dir = tempfile::tempdir().expect("temp dir");
    let backend_db = dir.path().join("backend.db");
    let owner = StoreOwner::open(&backend_db, &DbKey::from(KEY), "wa-driver").expect("open");
    let store = owner.store();
    let mut device = Device::new();
    device.pn = Some(format!("{PN}:3@s.whatsapp.net").parse().expect("jid"));
    device.lid = Some(format!("{LID}@lid").parse().expect("jid"));
    store.save(&device).await.expect("save");
    let clock = FakeClock::new(NOW);
    let wake = Arc::new(Notify::new());
    let outbox_wake = Arc::new(Notify::new());
    let stage = Arc::new(LazyStage::new(&backend_db, &DbKey::from(KEY)));
    let stage2 = Arc::new(Stage2::new(
        store.clone(),
        Arc::new(clock.clone()),
        outbox_wake.clone(),
        stage2::Timing::default(),
    ));
    let (acked, _) = watch::channel(0);
    Rig {
        capture: Capture::new(stage.clone(), wake.clone()),
        dir,
        _owner: owner,
        store,
        stage,
        wake,
        outbox_wake,
        merges: Arc::new(Notify::new()),
        acked,
        clock,
        stage2,
    }
}

impl Rig {
    fn driver(&self) -> Driver {
        Driver::spawn(Parts {
            store: self.store.clone(),
            stage: self.stage.clone(),
            stage2: self.stage2.clone(),
            clock: Arc::new(self.clock.clone()),
            wake: self.wake.clone(),
            outbox_wake: self.outbox_wake.clone(),
            merges: self.merges.clone(),
            acked: self.acked.subscribe(),
            timing: Timing::DEFAULT,
        })
    }

    /// Captures a chunk of one chat with `count` texts the way the hook does.
    async fn capture(&self, id: &str, count: usize) {
        let chunk = Chunk::new(SyncType::INITIAL_BOOTSTRAP, 0, 100).chat(Chat::new(PEER).texts(
            "A",
            1_700_000_100,
            count,
        ));
        self.capture
            .commit(LID, id, Some(0), chunk.blob())
            .await
            .expect("captured");
    }

    /// The core answers every row of `core_outbox` and says so; the locals of the messages it was given.
    async fn ack_all(&self) -> Vec<String> {
        let rows = self
            .store
            .read(|conn| outbox::peek(conn, 1000).map_err(|_| crate::error::database("peek")))
            .await
            .expect("peek");
        let mut locals = Vec::new();
        for row in rows {
            let seq = row.seq;
            for event in decode_outbox_row(&row.envelope).expect("decodes") {
                if let BackendEvent::HistoryBatch { messages, .. } = event {
                    locals.extend(messages.iter().map(|m| m.local.to_string()));
                }
            }
            self.store
                .write("test core", move |tx| {
                    tx.execute("DELETE FROM core_outbox WHERE seq = ?1", [seq])
                        .map(drop)
                        .map_err(sql("ack"))
                })
                .await
                .expect("ack");
            self.acked.send_modify(|n| *n += 1);
        }
        locals
    }

    /// The core answers until `want` messages were given to it (20 s at most); what it was given.
    async fn until_given(&self, want: usize) -> Vec<String> {
        let mut given = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        while given.len() < want {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the core was given {} of {want} messages",
                given.len()
            );
            given.extend(self.ack_all().await);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        given
    }

    async fn rows(&self) -> usize {
        self.store
            .read(|conn| {
                outbox::peek(conn, 1000)
                    .map(|rows| rows.len())
                    .map_err(|_| crate::error::database("peek"))
            })
            .await
            .expect("peek")
    }

    /// How many chunks the books record as delivered.
    async fn recorded(&self) -> i64 {
        self.store
            .read(|conn| {
                conn.query_row("SELECT count(*) FROM history_chunks", [], |r| r.get(0))
                    .map_err(sql("chunks"))
            })
            .await
            .expect("chunks")
    }

    async fn number(&self, sql_text: &'static str) -> i64 {
        self.store
            .read(move |conn| {
                conn.query_row(sql_text, [], |r| r.get(0))
                    .map_err(sql("count"))
            })
            .await
            .expect("count")
    }

    /// The handle of the stage the capture or the driver opened (cached: this does not open it).
    fn stage_handle(&self) -> crate::history::stage::StageHandle {
        self.stage.handle(LID).expect("the stage is open")
    }

    async fn cursor_chunk(&self) -> i64 {
        self.store
            .read(|conn| {
                conn.query_row("SELECT chunk FROM history_cursor WHERE id = 1", [], |r| {
                    r.get(0)
                })
                .map_err(sql("cursor"))
            })
            .await
            .expect("cursor")
    }
}

async fn stopped(mut driver: Driver) {
    tokio::time::timeout(Duration::from_secs(10), driver.stop())
        .await
        .expect("the driver ends when it is told");
}

#[tokio::test]
async fn a_captured_chunk_reaches_the_core_and_nothing_but_the_capture_woke_the_driver() {
    let rig = rig().await;
    let driver = rig.driver();
    // No stage file yet: the driver sleeps. The capture makes the file and wakes it.
    assert!(!rig.stage.on_disk());
    rig.capture("n1", 5).await;
    // The window fills: two rows, and the core has answered nothing.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while rig.rows().await < 2 {
        assert!(tokio::time::Instant::now() < deadline, "no rows came");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(rig.rows().await, 2, "the window holds at two rows");
    // The core answers: only that answer (the clock does not move) tells the driver to go on.
    let mut given = rig.until_given(5).await;
    given.sort();
    given.dedup();
    assert_eq!(given.len(), 5, "every message once: {given:?}");
    // The rows behind the first two (the chunk's end) follow the answers, so the chunk is recorded as delivered.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while rig.recorded().await < 1 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the driver did not go on after the core's answers"
        );
        rig.ack_all().await;
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    stopped(driver).await;
}

#[tokio::test]
async fn nothing_is_appended_while_the_link_hold_is_closed() {
    let rig = rig().await;
    rig.store.hold().set_closed(true);
    let driver = rig.driver();
    rig.capture("n1", 5).await;
    // The driver has been told of the chunk; with the hold closed it must not touch `core_outbox`.
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        rig.rows().await,
        0,
        "a row reached the core before AccountReady"
    );
    assert_eq!(rig.cursor_chunk().await, 0, "the cursor did not move");
    // The hold opens and the actor wakes the driver: the chunk is delivered.
    rig.store.hold().set_closed(false);
    rig.wake.notify_one();
    assert_eq!(rig.until_given(5).await.len(), 5);
    stopped(driver).await;
}

#[tokio::test]
async fn a_driver_over_a_stage_that_was_there_before_it_starts_with_the_backlog() {
    let rig = rig().await;
    // A chunk captured by an earlier run: the file is on disk when the driver starts, and no wake comes.
    rig.capture("n1", 3).await;
    assert!(rig.stage.on_disk());
    let driver = rig.driver();
    assert_eq!(rig.until_given(3).await.len(), 3);
    stopped(driver).await;
}

/// A stage of ANOTHER owner is on disk with chunks nobody delivered, and the books point into it: the open
/// replaces it, the driver starts the books over (counting the chunks that went with the file) and the new, empty
/// stage is delivered from its first chunk, as if there had been no other.
#[tokio::test]
async fn a_replaced_stage_starts_the_books_over_and_the_new_one_is_delivered_from_its_first_chunk()
{
    let rig = rig().await;
    let path = rig.dir.path().join("history").join("stage.db");
    let foreign = StageOwner::open(
        &path,
        &derive_key(&DbKey::from(KEY)),
        "someone else",
        &MIGRATIONS,
    )
    .expect("a foreign stage")
    .0;
    for (id, state) in [("old1", "captured"), ("old2", "planned"), ("old3", "done")] {
        foreign
            .handle()
            .write_at(Priority::High, "test insert", None, move |tx| {
                tx.execute(
                    "INSERT INTO chunks (msg_id, captured_at, size, state) VALUES (?1, 1, 3, ?2)",
                    (id, state),
                )
                .map_err(sql("test insert"))?;
                tx.execute(
                    "INSERT INTO chunk_bytes (seq, bytes) VALUES (?1, x'010203')",
                    [tx.last_insert_rowid()],
                )
                .map(drop)
                .map_err(sql("test insert"))
            })
            .await
            .expect("insert");
    }
    foreign.close();
    // The books stand in the middle of that stage's chunk 5, and its chunk 1 reads as delivered: a chunk 1 of the
    // new stage would be taken for an old one and its first row would not begin where the cursor stands.
    rig.store
        .write("test books", |tx| {
            tx.execute_batch(
                "UPDATE history_cursor SET chunk = 5, phase = 2, conv = 1, msg = 4 WHERE id = 1; \
                 INSERT INTO history_chunks(chunk) VALUES (1), (5); \
                 INSERT INTO history_announced(msg_id, announced_at) VALUES ('old1', 1);",
            )
            .map_err(sql("test books"))
        })
        .await
        .expect("books");
    let driver = rig.driver();
    rig.capture("n1", 3).await;
    let mut given = rig.until_given(3).await;
    given.sort();
    given.dedup();
    assert_eq!(
        given.len(),
        3,
        "the new stage's chunk is delivered: {given:?}"
    );
    // Its end row follows; only then is the chunk recorded, and only chunk 1 of the new stage is on the books.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while rig.recorded().await != 1
        || rig
            .number("SELECT coalesce(min(chunk), 0) FROM history_chunks")
            .await
            != 1
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the old stage's record of chunks was not forgotten"
        );
        rig.ack_all().await;
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        rig.number("SELECT lost FROM history_counts").await,
        2,
        "the two chunks of the replaced stage that were not delivered are counted lost"
    );
    assert_eq!(
        rig.number("SELECT count(*) FROM history_announced").await,
        0,
        "a notice of the replaced stage is not held against the new one"
    );
    stopped(driver).await;
}

/// MEDIUM-4: a chunk whose notice was counted lost and that the phone then sent after all takes the count back right
/// behind its capture (the capture's wake carries it): the notice is not lost, the lost count and the told count go
/// down, and the chunk is delivered like any other.
/// Mutant for the re-check: the `take_back_late` call removed from `steps`.
#[tokio::test]
async fn a_chunk_that_comes_after_its_notice_was_counted_lost_takes_the_count_back() {
    let rig = rig().await;
    rig.store
        .write("test books", |tx| {
            tx.execute_batch(
                "INSERT INTO history_announced(msg_id, announced_at, lost) VALUES ('n1', 1, 1), ('n2', 1, 1); \
                 UPDATE history_counts SET lost = 2, lost_told = 2 WHERE id = 1;",
            )
            .map_err(sql("test books"))
        })
        .await
        .expect("books");
    let driver = rig.driver();
    // The phone sends n1 after all (the driver starts when the stage file exists).
    rig.capture("n1", 2).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while rig.number("SELECT lost FROM history_counts").await != 1 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the count of the late chunk was not taken back"
        );
        rig.ack_all().await;
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        rig.number("SELECT lost_told FROM history_counts").await,
        1,
        "the told count is clamped to the new lost count"
    );
    assert_eq!(
        rig.number("SELECT lost FROM history_announced WHERE msg_id = 'n1'")
            .await,
        0
    );
    assert_eq!(
        rig.number("SELECT lost FROM history_announced WHERE msg_id = 'n2'")
            .await,
        1,
        "the chunk that did not come is still lost"
    );
    let mut given = rig.until_given(2).await;
    given.sort();
    given.dedup();
    assert_eq!(given.len(), 2, "the late chunk is delivered: {given:?}");
    stopped(driver).await;
}

/// The core's answers settle every row of a delivered chunk: its stage row is retired (the bytes are gone), its
/// record in `backend.db` says so, and the stage file stays.
#[tokio::test]
async fn a_delivered_chunk_is_retired_once_the_core_has_answered_every_row() {
    let rig = rig().await;
    let driver = rig.driver();
    rig.capture("n1", 5).await;
    let stage = rig.stage_handle();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        rig.ack_all().await;
        let look = stage
            .reader()
            .expect("a reader")
            .query_row(
                "SELECT state, EXISTS (SELECT 1 FROM chunk_bytes WHERE seq = 1) FROM chunks WHERE seq = 1",
                [],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, bool>(1)?)),
            )
            .ok();
        if look == Some(("done".to_owned(), false)) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the chunk was not retired: {look:?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while rig
        .number("SELECT retired FROM history_chunks WHERE chunk = 1")
        .await
        != 1
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "backend.db does not say the chunk is retired"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        rig.stage.on_disk(),
        "the stage stays when every chunk is done"
    );
    stopped(driver).await;
}

impl Rig {
    /// Keeps a text of the chat as a build whose map was one version older would have (it deferred it).
    async fn keep_old(&self, n: i64, local: &str, at: u64) {
        self.keep_old_in(PEER, n, local, at).await;
    }

    async fn keep_old_in(&self, chat: &'static str, n: i64, local: &str, at: u64) {
        let bytes = Msg::text(chat, local, at, "old").info.encode_to_vec();
        let version = crate::map::MAP_VERSION - 1;
        self.store
            .write("test setup", move |tx| {
                tx.execute(
                    "INSERT INTO history_deferred(chunk_msg_id, n, conv, item, map_version, kind) \
                     VALUES ('n0', ?1, ?2, ?3, ?4, 0)",
                    (n, chat, bytes, version),
                )
                .map(drop)
                .map_err(sql("setup"))
            })
            .await
            .expect("setup");
    }
}

/// C19: the driver's first act, before the stage's rows, is the take-up of what an earlier map deferred.
#[tokio::test]
async fn the_driver_takes_up_what_an_earlier_map_deferred_before_the_stage_rows() {
    let rig = rig().await;
    rig.keep_old(0, "D1", 1_600_000_000).await;
    rig.keep_old(1, "D2", 1_600_000_001).await;
    rig.capture("n1", 3).await;
    let driver = rig.driver();
    let given = rig.until_given(5).await;
    assert_eq!(
        &given[..2],
        ["D1", "D2"],
        "the take-up row has the lowest place: {given:?}"
    );
    assert_eq!(given.len(), 5, "and the chunk follows, once: {given:?}");
    assert_eq!(
        rig.number("SELECT count(*) FROM history_deferred").await,
        0,
        "nothing deferred is left to take up"
    );
    stopped(driver).await;
}

/// The take-up is a row of `core_outbox` like any other: while the link hold is closed (no `AccountReady` yet)
/// it waits for the actor's wake.
#[tokio::test]
async fn deferred_history_waits_for_the_link_hold() {
    let rig = rig().await;
    rig.store.hold().set_closed(true);
    rig.keep_old(0, "D1", 1_600_000_000).await;
    rig.capture("n1", 1).await;
    let driver = rig.driver();
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        rig.rows().await,
        0,
        "a row reached the core before AccountReady"
    );
    rig.store.hold().set_closed(false);
    rig.wake.notify_one();
    let given = rig.until_given(2).await;
    assert_eq!(given[0], "D1");
    stopped(driver).await;
}

/// The hold can close while the take-up waits for the core's answers (a relink): the answers come, and nothing more
/// is appended until the hold opens again.
#[tokio::test]
async fn deferred_history_stops_when_the_link_hold_closes_midway() {
    const CHATS: [&str; 3] = [
        "15550100102@s.whatsapp.net",
        "15550100103@s.whatsapp.net",
        "15550100104@s.whatsapp.net",
    ];
    let rig = rig().await;
    for (n, chat) in CHATS.into_iter().enumerate() {
        rig.keep_old_in(
            chat,
            i64::try_from(n).expect("small"),
            &format!("D{n}"),
            1_600_000_000,
        )
        .await;
    }
    rig.capture("n1", 1).await;
    let driver = rig.driver();
    // One row a chat: two rows fill the window, the third waits for the core.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while rig.rows().await < 2 {
        assert!(tokio::time::Instant::now() < deadline, "no rows came");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(rig.rows().await, 2, "the window holds at two rows");
    rig.store.hold().set_closed(true);
    assert_eq!(rig.ack_all().await, ["D0", "D1"]);
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        rig.rows().await,
        0,
        "a row reached the core with the hold closed"
    );
    rig.store.hold().set_closed(false);
    rig.wake.notify_one();
    let given = rig.until_given(2).await;
    assert_eq!(
        given[0], "D2",
        "the third chat follows once the hold opens: {given:?}"
    );
    stopped(driver).await;
}

/// HIGH-1 (the durability review's case): an earlier run's open REPLACED a foreign stage (the replacement is on disk at
/// once) and that run ended before the driver's `begin` started the books over (a kill, a quick quit after
/// `count_at_start` opened the stage). The next run finds a stage of its own owner and, before, took the books for the
/// new stage's: a new chunk 1 in the old chunk 5's place, never delivered. The core is given 3 of 3.
/// Mutant for the re-check: the id check removed from `retention::adopt` (it answers `Same`).
#[tokio::test]
async fn a_replacement_whose_book_reset_was_cut_off_is_still_delivered() {
    let rig = rig().await;
    let path = rig.dir.path().join("history").join("stage.db");
    let foreign = StageOwner::open(
        &path,
        &derive_key(&DbKey::from(KEY)),
        "someone else",
        &MIGRATIONS,
    )
    .expect("a foreign stage")
    .0;
    foreign
        .handle()
        .write_at(Priority::High, "test insert", None, |tx| {
            tx.execute(
                "INSERT INTO chunks (msg_id, captured_at, size, state) VALUES ('old1', 1, 3, 'captured')",
                [],
            )
            .map_err(sql("test insert"))?;
            tx.execute(
                "INSERT INTO chunk_bytes (seq, bytes) VALUES (1, x'010203')",
                [],
            )
            .map(drop)
            .map_err(sql("test insert"))
        })
        .await
        .expect("insert");
    foreign.close();
    rig.store
        .write("test books", |tx| {
            tx.execute_batch(
                "UPDATE history_cursor SET chunk = 5, phase = 2, conv = 1, msg = 4 WHERE id = 1; \
                 INSERT INTO history_chunks(chunk) VALUES (1), (5);",
            )
            .map_err(sql("test books"))
        })
        .await
        .expect("books");
    // The earlier run: it opens the stage (as `lost::count_at_start` or the capture does) and ends before any reset.
    let backend_db = rig.dir.path().join("backend.db");
    let origin = tokio::task::spawn_blocking(move || {
        let earlier = LazyStage::new(&backend_db, &DbKey::from(KEY));
        let _ = earlier.handle(LID).expect("open");
        let origin = earlier.origin();
        earlier.close();
        origin
    })
    .await
    .expect("joined");
    assert!(
        matches!(
            origin,
            Some(crate::history::stage::Origin::Foreign { lost: 1 })
        ),
        "{origin:?}"
    );
    // This run.
    let driver = rig.driver();
    rig.capture("n1", 3).await;
    let given = rig.until_given(3).await;
    assert_eq!(given.len(), 3);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while rig
        .number("SELECT coalesce(min(chunk), 0) FROM history_chunks")
        .await
        != 1
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the old stage's record of chunks was not forgotten"
        );
        rig.ack_all().await;
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    stopped(driver).await;
}

/// HIGH-1: a stage file that VANISHED and was made again (its directory removed by hand or by a failed disk) has an
/// incarnation the books do not hold: they start over before the new chunk is delivered.
/// Mutant for the re-check: the id check removed from `retention::adopt`.
#[tokio::test]
async fn a_stage_that_vanished_and_was_made_again_starts_the_books_over() {
    let rig = rig().await;
    rig.store
        .write("test books", |tx| {
            tx.execute_batch(
                "UPDATE history_cursor SET chunk = 5, phase = 2, conv = 1, msg = 4, \
                   stage = x'0a0b0c0d0e0f00010203040506070809' WHERE id = 1; \
                 INSERT INTO history_chunks(chunk) VALUES (1), (5);",
            )
            .map_err(sql("test books"))
        })
        .await
        .expect("books");
    let driver = rig.driver();
    rig.capture("n1", 3).await;
    assert_eq!(rig.until_given(3).await.len(), 3);
    stopped(driver).await;
}

const CALL: &str = "3EB0F1C2A3B4D5E6";

/// A call-log record of the phone's history: the peer called and it was missed.
fn missed_call(id: &str, at: i64) -> wa::CallLogRecord {
    let mut record = wa::CallLogRecord::default();
    record.call_id = Some(id.to_owned());
    record.call_creator_jid = Some(PEER.to_owned());
    record.start_time = Some(at);
    record.call_result = Some(wa::call_log_record::CallResult::MISSED);
    record
}

/// W6a1 D13, the whole road: a chunk that carries a call-log record is delivered, the record is KEPT by the delivery
/// zone, and once the push is over (the chunk's chat ended) it becomes a quiet call line in the chat, after the chunk's
/// own rows. The record stays, marked, so a later start does not place it again.
#[tokio::test]
async fn the_call_log_a_push_kept_is_taken_up_after_the_chunk() {
    let rig = rig().await;
    let mut chunk = Chunk::new(SyncType::INITIAL_BOOTSTRAP, 0, 100).chat(Chat::new(PEER).texts(
        "A",
        1_700_000_100,
        2,
    ));
    chunk
        .sync
        .call_log_records
        .push(missed_call(CALL, 1_700_000_050));
    rig.capture
        .commit(LID, "n1", Some(0), chunk.blob())
        .await
        .expect("captured");
    let driver = rig.driver();
    let given = rig.until_given(3).await;
    assert_eq!(
        given
            .iter()
            .filter(|local| local.starts_with("call:"))
            .count(),
        1,
        "one call line: {given:?}"
    );
    assert_eq!(
        given.last().map(String::as_str),
        Some("call:3EB0F1C2A3B4D5E6"),
        "the line follows the chunk's own messages: {given:?}"
    );
    assert_eq!(
        rig.number(
            "SELECT count(*) FROM history_deferred WHERE kind = 1 AND map_version = 9223372036854775807"
        )
        .await,
        1,
        "the record is kept and marked taken up"
    );
    stopped(driver).await;

    // A later start finds it taken up: nothing more is delivered.
    let driver = rig.driver();
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(
        rig.ack_all()
            .await
            .iter()
            .all(|local| !local.starts_with("call:")),
        "a record is placed once"
    );
    stopped(driver).await;
}

/// The take-up waits for the end of the push: while a chunk the phone announced is still to come (the look that keeps
/// the driver's 30 s poll), the kept records stay; the moment it is counted lost the push is over and they go.
#[tokio::test]
async fn the_call_log_waits_while_a_chunk_is_still_to_come() {
    let rig = rig().await;
    rig.store
        .write("test setup", |tx| {
            tx.execute_batch(
                "INSERT INTO history_announced(msg_id, announced_at, lost) VALUES ('x1', 1, 0), ('x2', 1, 0);",
            )
            .map_err(sql("setup"))?;
            tx.execute(
                "INSERT INTO history_deferred(chunk_msg_id, n, conv, item, map_version, kind) \
                 VALUES ('n0', 0, '', ?1, ?2, 1)",
                (
                    missed_call(CALL, 1_700_000_050).encode_to_vec(),
                    crate::map::MAP_VERSION,
                ),
            )
            .map(drop)
            .map_err(sql("setup"))
        })
        .await
        .expect("setup");
    rig.capture("n1", 1).await;
    let driver = rig.driver();
    let given = rig.until_given(1).await;
    assert!(!given[0].starts_with("call:"), "{given:?}");
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(
        rig.ack_all()
            .await
            .iter()
            .all(|local| !local.starts_with("call:")),
        "a call line went out while the push was open"
    );
    assert_eq!(
        rig.number("SELECT count(*) FROM history_deferred WHERE kind = 1 AND map_version < 9223372036854775807")
            .await,
        1,
        "the record waits"
    );
    // Both announced chunks are counted lost: nothing can still arrive.
    rig.store
        .write("test setup", |tx| {
            tx.execute("UPDATE history_announced SET lost = 1", [])
                .map(drop)
                .map_err(sql("setup"))
        })
        .await
        .expect("lost");
    rig.wake.notify_one();
    let given = rig.until_given(1).await;
    assert!(
        given.iter().any(|local| local == "call:3EB0F1C2A3B4D5E6"),
        "the line comes once the push is over: {given:?}"
    );
    stopped(driver).await;
}

/// W6a2, B9: the people a push delivered are known to the core once it is over, and the pairs the store holds for them are
/// due then: the driver wakes the merge sweep at its quiet step with nothing still to come, and not while a chunk the phone
/// announced is still to come. Mutants: no wake; a wake while the push is open.
#[tokio::test]
async fn the_merge_sweep_is_woken_when_the_push_is_over_and_not_before() {
    let rig = rig().await;
    rig.store
        .write("test setup", |tx| {
            tx.execute_batch(
                "INSERT INTO history_announced(msg_id, announced_at, lost) VALUES ('x1', 1, 0), ('x2', 1, 0);",
            )
            .map_err(sql("setup"))
        })
        .await
        .expect("setup");
    rig.capture("n1", 1).await;
    let driver = rig.driver();
    let _ = rig.until_given(1).await;
    // The core answers every row, so the driver is at its quiet step; a chunk is still to come, so the push is open.
    rig.ack_all().await;
    assert!(
        tokio::time::timeout(Duration::from_millis(600), rig.merges.notified())
            .await
            .is_err(),
        "the sweep was woken while a chunk is still to come"
    );
    // Both announced chunks are counted lost: nothing can still arrive.
    rig.store
        .write("test setup", |tx| {
            tx.execute("UPDATE history_announced SET lost = 1", [])
                .map(drop)
                .map_err(sql("setup"))
        })
        .await
        .expect("lost");
    rig.wake.notify_one();
    let woken = async {
        loop {
            rig.ack_all().await;
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    tokio::select! {
        () = rig.merges.notified() => {}
        () = woken => unreachable!("the loop does not end"),
        () = tokio::time::sleep(Duration::from_secs(10)) => panic!("the sweep was not woken when the push was over"),
    }
    stopped(driver).await;
}
