//! The lost-history count and the quit's wait over a real store and a real stage (W4 C20). Every id is synthetic.

use std::sync::Arc;
use std::time::Duration;

use kourier_backend_api::{BackendEvent, BackendNotice, Clock, FakeClock};
use kourier_db::{DbKey, Priority};
use wacore::store::device::Device;
use wacore::store::traits::DeviceStore as _;

use super::{
    REUPLOAD_HOPE_MS, Waited, count_at_start, mark_lost, owed, take_back, wait_for_captures,
};
use crate::error::sql;
use crate::history::stage::{LazyStage, StageHandle};
use crate::store::{KourierStore, StoreOwner};

const KEY: [u8; 32] = [0x66; 32];
/// A time long after every notice the tests announce by default (those are announced at ms 1, so they are older than
/// the 24 h a re-upload is hoped for).
const NOW: i64 = 100 * 24 * 60 * 60 * 1000;
const LID: &str = "99900100123";
const PN: &str = "99900100456";

struct Rig {
    _dir: tempfile::TempDir,
    _owner: StoreOwner,
    store: KourierStore,
    lazy: Arc<LazyStage>,
}

async fn rig() -> Rig {
    let dir = tempfile::tempdir().expect("temp dir");
    let backend_db = dir.path().join("backend.db");
    let owner = StoreOwner::open(&backend_db, &DbKey::from(KEY), "wa-lost").expect("open");
    let store = owner.store();
    let mut device = Device::new();
    device.pn = Some(format!("{PN}:3@s.whatsapp.net").parse().expect("jid"));
    device.lid = Some(format!("{LID}@lid").parse().expect("jid"));
    store.save(&device).await.expect("save the device");
    let lazy = Arc::new(LazyStage::new(&backend_db, &DbKey::from(KEY)));
    Rig {
        _dir: dir,
        _owner: owner,
        store,
        lazy,
    }
}

impl Rig {
    /// The stage, made now (a capture would have made it).
    fn stage(&self) -> StageHandle {
        self.lazy.handle(LID).expect("the stage opens")
    }

    /// Stage 2 recorded the notice of `id`, long ago.
    async fn announce(&self, id: &'static str) {
        self.announce_at(id, 1).await;
    }

    /// Stage 2 recorded the notice of `id` at `at` (ms).
    async fn announce_at(&self, id: &'static str, at: i64) {
        self.store
            .write("test announce", move |tx| {
                tx.execute(
                    "INSERT INTO history_announced(msg_id, announced_at) VALUES (?1, ?2)",
                    (id, at),
                )
                .map_err(sql("test announce"))?;
                Ok(())
            })
            .await
            .expect("announce");
    }

    /// The capture kept the chunk of `id` in the stage, in `state`.
    async fn capture(&self, id: &'static str, state: &'static str) {
        self.capture_at(id, state, 1, None).await;
    }

    /// The capture kept the chunk of `id` at `at` (ms), whose progress the planning pass read as `progress`.
    async fn capture_at(
        &self,
        id: &'static str,
        state: &'static str,
        at: i64,
        progress: Option<i64>,
    ) {
        self.stage()
            .write_at(Priority::High, "test capture", None, move |tx| {
                tx.execute(
                    "INSERT INTO chunks (msg_id, captured_at, size, state, progress) VALUES (?1, ?3, 3, ?2, ?4)",
                    (id, state, at, progress),
                )
                .map_err(sql("test capture"))?;
                tx.execute(
                    "INSERT INTO chunk_bytes (seq, bytes) VALUES (?1, x'010203')",
                    [tx.last_insert_rowid()],
                )
                .map_err(sql("test capture"))?;
                Ok(())
            })
            .await
            .expect("capture");
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
}

#[tokio::test]
async fn an_announced_chunk_never_captured_is_counted_lost_at_the_next_start() {
    let rig = rig().await;
    rig.capture("n1", "planned").await;
    for id in ["n1", "n2", "n3"] {
        rig.announce(id).await;
    }

    assert_eq!(
        count_at_start(&rig.store, &rig.lazy, NOW)
            .await
            .expect("count"),
        2,
        "n2 and n3 were announced and the stage never held them"
    );
    assert_eq!(rig.number("SELECT lost FROM history_counts").await, 2);
    assert_eq!(
        rig.number("SELECT count(*) FROM history_announced WHERE lost = 1")
            .await,
        2
    );
    assert_eq!(
        rig.number("SELECT lost FROM history_announced WHERE msg_id = 'n1'")
            .await,
        0,
        "a captured chunk is never lost"
    );
    // The next look finds nothing new: a notice is counted once.
    assert_eq!(
        count_at_start(&rig.store, &rig.lazy, NOW)
            .await
            .expect("again"),
        0
    );
    assert_eq!(rig.number("SELECT lost FROM history_counts").await, 2);
}

#[tokio::test]
async fn a_chunk_the_stage_holds_in_any_state_is_never_lost() {
    let rig = rig().await;
    for (id, state) in [
        ("a", "captured"),
        ("b", "planned"),
        ("c", "done"),
        ("d", "bad"),
    ] {
        rig.capture(id, state).await;
        rig.announce(id).await;
    }
    assert_eq!(
        count_at_start(&rig.store, &rig.lazy, NOW)
            .await
            .expect("count"),
        0
    );
    assert_eq!(rig.number("SELECT lost FROM history_counts").await, 0);
}

#[tokio::test]
async fn with_no_stage_file_every_announced_chunk_is_lost_and_none_makes_the_file() {
    let rig = rig().await;
    rig.announce("n1").await;
    rig.announce("n2").await;
    assert!(!rig.lazy.on_disk());
    assert_eq!(
        count_at_start(&rig.store, &rig.lazy, NOW)
            .await
            .expect("count"),
        2
    );
    assert!(
        !rig.lazy.on_disk(),
        "counting touches no file: the stage is made by a capture"
    );
    assert_eq!(rig.number("SELECT lost FROM history_counts").await, 2);
}

#[tokio::test]
async fn nothing_announced_opens_nothing() {
    let rig = rig().await;
    assert_eq!(
        count_at_start(&rig.store, &rig.lazy, NOW)
            .await
            .expect("count"),
        0
    );
    assert!(!rig.lazy.on_disk());
    assert_eq!(rig.number("SELECT lost FROM history_counts").await, 0);
}

/// A stage of another owner is replaced at open: which of the announced chunks it held is unknowable, so the start's
/// count leaves them to the driver's reset (which counts what the replacement lost) instead of counting them twice.
#[tokio::test]
async fn a_replaced_stage_is_not_judged_here() {
    let rig = rig().await;
    // A stage file of another owner, on disk.
    let backend_db = rig._dir.path().join("backend.db");
    let other = LazyStage::new(&backend_db, &DbKey::from(KEY));
    other.handle("99900199999").expect("another owner's stage");
    other.close();
    rig.announce("n1").await;

    assert_eq!(
        count_at_start(&rig.store, &rig.lazy, NOW)
            .await
            .expect("count"),
        0
    );
    assert_eq!(rig.number("SELECT lost FROM history_counts").await, 0);
    assert_eq!(
        rig.number("SELECT lost FROM history_announced").await,
        0,
        "the notice is left for the reset"
    );
}

/// HIGH-1: books that point into another incarnation describe a file that is gone (an earlier run replaced it and
/// ended before the driver started them over): what the stage lacks says nothing about a loss, so nothing is counted
/// and the notices are left for the driver's adoption.
/// Mutant for the re-check: the `stale` check taken out of `missing`.
#[tokio::test]
async fn books_that_point_into_another_stage_are_not_judged_here() {
    let rig = rig().await;
    rig.stage();
    rig.announce("n1").await;
    rig.store
        .write("test books", |tx| {
            tx.execute(
                "UPDATE history_cursor SET stage = x'01020304050607080102030405060708' WHERE id = 1",
                [],
            )
            .map_err(sql("test books"))?;
            Ok(())
        })
        .await
        .expect("books");
    assert_eq!(
        count_at_start(&rig.store, &rig.lazy, NOW)
            .await
            .expect("count"),
        0
    );
    assert_eq!(rig.number("SELECT lost FROM history_counts").await, 0);
    assert_eq!(rig.number("SELECT lost FROM history_announced").await, 0);
}

#[tokio::test]
async fn the_wait_is_none_when_nothing_is_missing_and_ends_with_the_capture_or_the_bound() {
    let rig = rig().await;
    let clock = FakeClock::new(1_000);
    let shared: Arc<dyn Clock> = Arc::new(clock.clone());
    // Nothing announced: no wait, no timer.
    assert_eq!(
        wait_for_captures(
            &rig.store,
            &rig.lazy,
            &shared,
            1_000,
            Duration::from_secs(15)
        )
        .await,
        Waited::Nothing
    );
    assert_eq!(clock.sleepers(), 0);

    rig.announce_at("n1", 1_000).await;
    assert_eq!(owed(&rig.store, &rig.lazy, 1_000).await.expect("owed"), 1);
    let waiter = {
        let (store, lazy, shared) = (rig.store.clone(), rig.lazy.clone(), shared.clone());
        tokio::spawn(async move {
            wait_for_captures(&store, &lazy, &shared, 1_000, Duration::from_secs(15)).await
        })
    };
    // It waits (the bound is not up and the chunk is not there).
    tokio::time::sleep(Duration::from_millis(350)).await;
    assert!(!waiter.is_finished());
    rig.capture("n1", "captured").await;
    assert_eq!(waiter.await.expect("joins"), Waited::Captured);

    // A chunk that never comes ends the wait at the bound, not before.
    rig.announce_at("n2", 1_000).await;
    let waiter = {
        let (store, lazy, shared) = (rig.store.clone(), rig.lazy.clone(), shared.clone());
        tokio::spawn(async move {
            wait_for_captures(&store, &lazy, &shared, 1_000, Duration::from_secs(15)).await
        })
    };
    let armed = clock.now_ms() + 15_000;
    while !clock.deadlines().contains(&armed) {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    clock.advance(Duration::from_secs(14));
    tokio::time::sleep(Duration::from_millis(350)).await;
    assert!(!waiter.is_finished(), "14 s of a 15 s bound");
    clock.advance(Duration::from_secs(1));
    assert_eq!(waiter.await.expect("joins"), Waited::TimedOut);
}

/// A notice marked lost by one counter is not counted again by another that read it before the mark (the zone's own
/// guard, beside the read that leaves marked notices out).
#[tokio::test]
async fn a_notice_marked_lost_twice_is_counted_once() {
    let rig = rig().await;
    rig.announce("n1").await;
    assert_eq!(
        mark_lost(&rig.store, vec!["n1".to_owned()])
            .await
            .expect("mark"),
        1
    );
    assert_eq!(
        mark_lost(&rig.store, vec!["n1".to_owned()])
            .await
            .expect("again"),
        0,
        "a marked notice is not counted again"
    );
    assert_eq!(rig.number("SELECT lost FROM history_counts").await, 1);
}

/// MEDIUM-4: the phone re-uploads a chunk that got no receipt, so a notice recorded a moment ago is not a loss at the
/// next start: it stays `lost = 0` (and "unresolved") until a day has passed.
/// Mutant for the re-check: the age test taken out of `count_at_start` (every missing notice counted at once).
#[tokio::test]
async fn a_recent_missing_notice_is_not_lost_at_the_next_start() {
    let rig = rig().await;
    let announced = NOW - 60 * 60 * 1000;
    rig.announce_at("n1", announced).await;
    assert_eq!(
        count_at_start(&rig.store, &rig.lazy, NOW)
            .await
            .expect("count"),
        0,
        "announced an hour ago: the phone may still send it again"
    );
    assert_eq!(rig.number("SELECT lost FROM history_counts").await, 0);
    assert_eq!(
        rig.number("SELECT lost FROM history_announced WHERE msg_id = 'n1'")
            .await,
        0
    );
    // Just short of a day: still not.
    assert_eq!(
        count_at_start(&rig.store, &rig.lazy, announced + REUPLOAD_HOPE_MS - 1)
            .await
            .expect("count"),
        0
    );
    // A day after the announcement it is a loss, once.
    assert_eq!(
        count_at_start(&rig.store, &rig.lazy, announced + REUPLOAD_HOPE_MS)
            .await
            .expect("count"),
        1
    );
    assert_eq!(rig.number("SELECT lost FROM history_counts").await, 1);
}

/// MEDIUM-4: a recent notice is a loss all the same when the phone's push has ended since (a chunk with the final
/// progress was captured after the announcement); one announced after the end is not.
/// Mutant for the re-check: the `final_at` arm taken out of `count_at_start`.
#[tokio::test]
async fn a_recent_missing_notice_is_lost_when_the_push_has_ended_since() {
    let rig = rig().await;
    let end = NOW - 10 * 60 * 1000;
    rig.announce_at("early", end - 5_000).await;
    rig.announce_at("late", end + 5_000).await;
    // A chunk that is not the last of its type does not end anything.
    rig.capture_at("first", "planned", end - 20_000, Some(40))
        .await;
    assert_eq!(
        count_at_start(&rig.store, &rig.lazy, NOW)
            .await
            .expect("count"),
        0,
        "the push has not ended: both may still come"
    );
    rig.capture_at("last", "planned", end, Some(100)).await;
    assert_eq!(
        count_at_start(&rig.store, &rig.lazy, NOW)
            .await
            .expect("count"),
        1,
        "the notice announced before the push ended is a loss now"
    );
    assert_eq!(
        rig.number("SELECT lost FROM history_announced WHERE msg_id = 'early'")
            .await,
        1
    );
    assert_eq!(
        rig.number("SELECT lost FROM history_announced WHERE msg_id = 'late'")
            .await,
        0,
        "one announced after the end may still come"
    );
}

/// MEDIUM-4: the quit waits only for what this run announced; a notice of an earlier run has no download left to wait
/// for (the connection that carried it is gone).
/// Mutant for the re-check: `owed` without the `since_ms` filter.
#[tokio::test]
async fn the_quit_does_not_wait_for_a_chunk_an_earlier_run_announced() {
    let rig = rig().await;
    rig.announce_at("old-run", NOW - 60_000).await;
    assert_eq!(
        owed(&rig.store, &rig.lazy, NOW).await.expect("owed"),
        0,
        "announced before this run began"
    );
    rig.announce_at("this-run", NOW + 5).await;
    assert_eq!(owed(&rig.store, &rig.lazy, NOW).await.expect("owed"), 1);
    let clock: Arc<dyn Clock> = Arc::new(FakeClock::new(u64::try_from(NOW).expect("positive")));
    rig.capture("this-run", "captured").await;
    assert_eq!(
        wait_for_captures(&rig.store, &rig.lazy, &clock, NOW, Duration::from_secs(15)).await,
        Waited::Nothing,
        "nothing of this run is missing: no wait at all"
    );
}

/// MEDIUM-4: a chunk that is captured after its notice was counted lost takes the count back, and the told count never
/// stays above the new lost count.
/// Mutant for the re-check: `take_back` without its `UPDATE history_counts` (or the clamp of `lost_told`).
#[tokio::test]
async fn a_late_capture_takes_the_count_back() {
    let rig = rig().await;
    for id in ["n1", "n2", "n3"] {
        rig.announce(id).await;
    }
    assert_eq!(
        count_at_start(&rig.store, &rig.lazy, NOW)
            .await
            .expect("count"),
        3
    );
    // The person was told of all three.
    rig.counts(3, 3).await;

    // Nothing arrived yet: nothing to take back, three still wait.
    let stage = rig.stage();
    let taken = take_back(&rig.store, &stage).await.expect("look");
    assert_eq!((taken.back, taken.waiting), (0, 3));
    assert_eq!(rig.number("SELECT lost FROM history_counts").await, 3);

    // The phone sent n2 after all.
    rig.capture("n2", "captured").await;
    let taken = take_back(&rig.store, &stage).await.expect("take back");
    assert_eq!((taken.back, taken.waiting), (1, 2));
    assert_eq!(rig.number("SELECT lost FROM history_counts").await, 2);
    assert_eq!(
        rig.number("SELECT lost_told FROM history_counts").await,
        2,
        "the told count is clamped to the new lost count"
    );
    assert_eq!(
        rig.number("SELECT lost FROM history_announced WHERE msg_id = 'n2'")
            .await,
        0
    );
    assert_eq!(
        rig.number("SELECT count(*) FROM history_announced WHERE lost = 1")
            .await,
        2
    );
    // The same look again changes nothing.
    let again = take_back(&rig.store, &stage).await.expect("again");
    assert_eq!((again.back, again.waiting), (0, 2));
    assert_eq!(rig.number("SELECT lost FROM history_counts").await, 2);

    // A person who was told of fewer than were lost keeps that count.
    rig.counts(2, 1).await;
    rig.capture("n3", "planned").await;
    let taken = take_back(&rig.store, &stage).await.expect("take back");
    assert_eq!(taken.back, 1);
    assert_eq!(rig.number("SELECT lost FROM history_counts").await, 1);
    assert_eq!(
        rig.number("SELECT lost_told FROM history_counts").await,
        1,
        "told 1 of 2 stays 1 of 1"
    );
}

// ----- the notice to the person (W4 C20b) -----

use super::{append_notice, mark_told, notice_owed};

impl Rig {
    async fn counts(&self, lost: i64, told: i64) {
        self.store
            .write("test counts", move |tx| {
                tx.execute(
                    "UPDATE history_counts SET lost = ?1, lost_told = ?2 WHERE id = 1",
                    (lost, told),
                )
                .map_err(sql("test counts"))?;
                Ok(())
            })
            .await
            .expect("counts");
    }
}

#[tokio::test]
async fn the_notice_is_owed_for_what_the_person_was_not_told_and_never_for_nothing() {
    let rig = rig().await;
    assert_eq!(
        notice_owed(&rig.store).await.expect("owed"),
        None,
        "nothing was lost"
    );
    rig.counts(3, 1).await;
    assert_eq!(notice_owed(&rig.store).await.expect("owed"), Some(2));
    rig.counts(3, 3).await;
    assert_eq!(
        notice_owed(&rig.store).await.expect("owed"),
        None,
        "all of it was told"
    );
}

#[tokio::test]
async fn telling_marks_no_more_than_was_lost() {
    let rig = rig().await;
    rig.counts(2, 0).await;
    mark_told(&rig.store, 1).await.expect("told");
    assert_eq!(rig.number("SELECT lost_told FROM history_counts").await, 1);
    mark_told(&rig.store, 5).await.expect("told");
    assert_eq!(
        rig.number("SELECT lost_told FROM history_counts").await,
        2,
        "the person cannot have been told of more than was lost"
    );
}

#[tokio::test]
async fn a_notice_row_is_appended_for_what_is_owed_and_none_when_nothing_is() {
    let rig = rig().await;
    assert_eq!(
        append_notice(&rig.store, 1_000).await.expect("append"),
        None
    );
    assert_eq!(rig.number("SELECT count(*) FROM core_outbox").await, 0);

    rig.counts(2, 0).await;
    let told = append_notice(&rig.store, 1_000)
        .await
        .expect("append")
        .expect("a notice");
    assert_eq!(told.count, 2);
    let rows = rig
        .store
        .read(|conn| kourier_db::outbox::peek(conn, 10).map_err(|_| crate::error::database("peek")))
        .await
        .expect("peek");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].seq, told.seq);
    let events = kourier_backend_api::decode_outbox_row(&rows[0].envelope).expect("decodes");
    assert!(
        matches!(
            events.as_slice(),
            [BackendEvent::Notice(BackendNotice::HistoryLost {
                count: 2
            })]
        ),
        "{events:?}"
    );
}
