//! The catch-up machine (W3a, chunk C7): what makes `Online`, how the boundary is taken and kept,
//! and what the machine waits for in the store. The actor-level behaviour (the signals, the timer,
//! the state the account shows) is in `actor::tests`.

use std::sync::Arc;

use kourier_backend_api::FakeClock;
use kourier_db::DbKey;

use super::*;
use crate::inbox::{Arrival, Inbox, InboxMeta, Origin};
use crate::store::StoreOwner;

const NOW: i64 = 1_800_000_000_000;

fn setup() -> (tempfile::TempDir, StoreOwner, Inbox) {
    let dir = tempfile::tempdir().expect("temp dir");
    let owner = StoreOwner::open(
        &dir.path().join("backend.db"),
        &DbKey::from([5u8; 32]),
        "wa-catchup",
    )
    .expect("open");
    let inbox = Inbox::new(
        owner.store(),
        Arc::new(FakeClock::new(u64::try_from(NOW).expect("time"))),
    );
    (dir, owner, inbox)
}

fn arrival(id: &str) -> Arrival {
    Arrival {
        chat: "31000000001@s.whatsapp.net".into(),
        sender: "31000000001@s.whatsapp.net".into(),
        id: id.into(),
        origin: Origin::E2e,
        message: vec![1, 2, 3],
        meta: InboxMeta {
            v: InboxMeta::VERSION,
            timestamp: 1_700_000_000,
            from_me: false,
            is_group: false,
            push_name: String::new(),
            lid_addressing: false,
            sender_alt: None,
            recipient_alt: None,
            ephemeral_expiration: None,
            is_offline: true,
        },
    }
}

async fn commit(inbox: &Inbox, ids: &[&str]) {
    inbox
        .commit(ids.iter().map(|id| arrival(id)).collect(), Vec::new())
        .await
        .expect("commit");
}

/// Runs a statement on the writer.
async fn sql_do(store: &KourierStore, statement: &'static str) {
    store
        .write("test: statement", move |tx| {
            tx.execute(statement, [])
                .map(drop)
                .map_err(crate::error::sql("test statement"))
        })
        .await
        .expect("statement");
}

async fn outbox_append(store: &KourierStore, rows: usize) {
    store
        .write("test: append", move |tx| {
            for _ in 0..rows {
                kourier_db::outbox::append(tx, b"row", NOW)
                    .map_err(|_| crate::error::database("append"))?;
            }
            Ok(())
        })
        .await
        .expect("append");
}

async fn outbox_delete_through(store: &KourierStore, seq: i64) {
    store
        .write("test: delete", move |tx| {
            kourier_db::outbox::delete_through(tx, seq)
                .map(drop)
                .map_err(|_| crate::error::database("delete"))
        })
        .await
        .expect("delete");
}

/// `Online` needs the connection AND the drain, in whichever order upstream brings them; a
/// connection without a drain is `Syncing`, a drain without `Connected` is `Syncing`, nothing at all
/// says nothing.
#[test]
fn online_needs_the_connection_and_the_drain_in_either_order() {
    let mut m = CatchUp::new();
    assert_eq!(m.shown(), Shown::Nothing);
    assert!(!m.is_up());

    // `Connected` first (a stuck tail): Syncing until the drain is through.
    m.connected();
    assert_eq!(m.shown(), Shown::Syncing);
    m.boundary_taken(3);
    assert_eq!(m.shown(), Shown::Syncing);
    m.phase = Phase::Core { target: 1 };
    assert_eq!(m.shown(), Shown::Syncing);
    m.phase = Phase::Through;
    assert_eq!(m.shown(), Shown::Online);

    // The drain first (upstream's normal order): Syncing until `Connected`.
    let mut m = CatchUp::new();
    m.previewed(0);
    assert_eq!(m.shown(), Shown::Syncing);
    m.boundary_taken(0);
    m.phase = Phase::Through;
    assert_eq!(m.shown(), Shown::Syncing, "through, but not connected");
    m.connected();
    assert_eq!(m.shown(), Shown::Online);

    // The next connection starts over.
    m.reset();
    assert_eq!((m.shown(), m.phase()), (Shown::Nothing, Phase::Backlog));
}

/// D7: the ledger may be swept once per connection, and only when upstream said the drain is complete
/// AND Kourier's side is through it. A drain Kourier took the boundary of by its own timeout (no
/// completion) offers nothing, and neither does a connection whose starting counter is unknown.
#[test]
fn a_ledger_sweep_is_offered_once_and_only_after_a_complete_durable_drain() {
    let mut m = CatchUp::new();
    m.cover(7);
    m.connected();
    assert_eq!(m.take_sweep(), None, "backlog");
    m.boundary_taken(3);
    m.drain_over(0);
    assert_eq!(m.take_sweep(), None, "stage 2 is not through");
    m.phase = Phase::Core { target: 1 };
    assert_eq!(m.take_sweep(), None, "the core has not acknowledged");
    m.phase = Phase::Through;
    assert_eq!(m.take_sweep(), Some(7));
    assert_eq!(m.take_sweep(), None, "once per connection");

    // The boundary taken by the machine's own timeout: upstream never said the drain is complete.
    let mut m = CatchUp::new();
    m.cover(7);
    m.connected();
    m.boundary_taken(3);
    m.phase = Phase::Through;
    assert_eq!(m.take_sweep(), None, "no completion, no sweep");

    // A connection whose starting counter could not be read sweeps nothing, and a reset forgets it.
    let mut m = CatchUp::new();
    m.drain_over(0);
    m.phase = Phase::Through;
    assert_eq!(m.take_sweep(), None, "no counter, no sweep");
    m.cover(9);
    m.reset();
    m.drain_over(0);
    m.phase = Phase::Through;
    assert_eq!(m.take_sweep(), None, "a new connection starts over");
}

#[tokio::test]
async fn the_counter_is_the_highest_arrival_ever_given() {
    let (_dir, owner, inbox) = setup();
    let store = owner.store();
    assert_eq!(counter(&store).await.expect("read"), 0);
    commit(&inbox, &["A", "B", "C"]).await;
    assert_eq!(counter(&store).await.expect("read"), 3);
    sql_do(&store, "DELETE FROM inbox").await;
    assert_eq!(
        counter(&store).await.expect("read"),
        3,
        "numbers are never reused"
    );
    owner.close();
}

#[test]
fn the_first_boundary_of_a_drain_counts_and_a_new_drain_starts_over() {
    let mut m = CatchUp::new();
    m.connected();
    m.boundary_taken(3);
    assert_eq!(m.phase(), Phase::Stage2 { boundary: 3 });
    assert!(m.waits_for_stage2());
    // A second completion of the same drain does not move it.
    m.boundary_taken(9);
    assert_eq!(m.phase(), Phase::Stage2 { boundary: 3 });
    // An announced drain (a reconnect that brings more) starts over, and so does a broken one.
    m.previewed(0);
    assert_eq!(m.phase(), Phase::Backlog);
    m.boundary_taken(4);
    m.interrupted();
    assert_eq!(m.phase(), Phase::Backlog);
    assert!(!m.waits_for_stage2());
}

/// The boundary is the arrival counter at the moment it is taken, forgetting clears it, and an
/// empty inbox has the boundary 0. The handler's detached forms take their place in the writer's
/// order (a read queued after them sees them).
#[tokio::test]
async fn the_boundary_is_the_arrival_counter_and_forgetting_clears_it() {
    let (_dir, owner, inbox) = setup();
    let store = owner.store();
    assert_eq!(boundary(&store).await.expect("read"), None);
    assert_eq!(take_boundary(&store).await.expect("take"), 0);
    commit(&inbox, &["A", "B"]).await;
    assert_eq!(take_boundary(&store).await.expect("take"), 2);
    assert_eq!(boundary(&store).await.expect("read"), Some(2));
    commit(&inbox, &["C"]).await;
    assert_eq!(boundary(&store).await.expect("read"), Some(2), "later rows");
    inbox.forget_boundary_detached().expect("queue");
    assert_eq!(boundary(&store).await.expect("read"), None);
    inbox.take_boundary_detached().expect("queue");
    assert_eq!(boundary(&store).await.expect("read"), Some(3));
    forget_boundary(&store).await.expect("forget");
    assert_eq!(boundary(&store).await.expect("read"), None);
    owner.close();
}

/// Stage 2 must be through the rows up to the boundary, not through later ones, and a parked row
/// (a content failure waiting for its retry) does not hold the account `Syncing`.
#[tokio::test]
async fn stage_2_must_be_through_the_rows_up_to_the_boundary_and_only_those() {
    let (_dir, owner, inbox) = setup();
    let store = owner.store();
    commit(&inbox, &["A", "B", "C"]).await;
    let boundary = take_boundary(&store).await.expect("take");
    assert_eq!(boundary, 3);
    commit(&inbox, &["D"]).await;
    let mut m = CatchUp::new();
    m.connected();
    m.boundary_taken(boundary);

    m.advance(&store, NOW).await.expect("advance");
    assert_eq!(
        m.phase(),
        Phase::Stage2 { boundary: 3 },
        "three rows are due"
    );

    // A and C are finished; B failed and waits for its retry: nothing due is left up to the boundary.
    // D (after the boundary) is still new and due, and does not matter.
    sql_do(
        &store,
        "UPDATE inbox SET state = 'done', message = NULL, meta = NULL WHERE id IN ('A', 'C')",
    )
    .await;
    m.advance(&store, NOW).await.expect("advance");
    assert_eq!(m.phase(), Phase::Stage2 { boundary: 3 }, "B is due still");
    sql_do(
        &store,
        "UPDATE inbox SET parked_until = 1900000000000 WHERE id = 'B'",
    )
    .await;
    m.advance(&store, NOW).await.expect("advance");
    assert_eq!(
        m.phase(),
        Phase::Through,
        "a parked row and a later row do not hold it"
    );
    assert_eq!(m.shown(), Shown::Online);
    owner.close();
}

/// Once stage 2 is through, the core must have acknowledged what was written until then (its rows
/// are deleted after the ack); rows written later do not hold `Online` back.
#[tokio::test]
async fn the_core_must_acknowledge_what_was_written_until_the_boundary_and_nothing_after() {
    let (_dir, owner, _inbox) = setup();
    let store = owner.store();
    outbox_append(&store, 2).await;
    let mut m = CatchUp::new();
    m.connected();
    m.boundary_taken(0);
    m.advance(&store, NOW).await.expect("advance");
    assert_eq!(
        m.phase(),
        Phase::Core { target: 2 },
        "two rows are with the core's side"
    );

    outbox_append(&store, 1).await;
    outbox_delete_through(&store, 1).await;
    m.advance(&store, NOW).await.expect("advance");
    assert_eq!(
        m.phase(),
        Phase::Core { target: 2 },
        "row 2 is not acknowledged yet"
    );
    outbox_delete_through(&store, 2).await;
    m.advance(&store, NOW).await.expect("advance");
    assert_eq!(
        m.phase(),
        Phase::Through,
        "row 3 was written after the mark"
    );
    assert_eq!(m.shown(), Shown::Online);
    owner.close();
}

/// An empty `core_outbox` has nothing to wait for, and a failed read leaves the phase as it was.
#[tokio::test]
async fn nothing_to_wait_for_is_online_at_once_and_a_failed_read_changes_nothing() {
    let (_dir, owner, _inbox) = setup();
    let store = owner.store();
    let mut m = CatchUp::new();
    m.connected();
    m.boundary_taken(0);
    m.advance(&store, NOW).await.expect("advance");
    assert_eq!(m.phase(), Phase::Through);

    let mut m = CatchUp::new();
    m.connected();
    m.boundary_taken(0);
    owner.close();
    assert!(m.advance(&store, NOW).await.is_err(), "the writer is gone");
    assert_eq!(m.phase(), Phase::Stage2 { boundary: 0 });
}

/// Fix F7: upstream's own 60 s wait ends the drain with the number of stanzas it had PROCESSED, below
/// the preview's total. That is not a completed drain, so it offers no ledger sweep; the real end
/// marker (the count reaches the total) and a drain with no preview do.
#[test]
fn a_completion_below_the_previews_total_is_not_a_completed_drain() {
    let through = |m: &mut CatchUp| {
        m.cover(7);
        m.connected();
        m.boundary_taken(3);
        m.phase = Phase::Through;
    };
    let mut timed_out = CatchUp::new();
    timed_out.previewed(10);
    timed_out.drain_over(3);
    through(&mut timed_out);
    assert!(timed_out.drain_ended(), "it was the drain's one end");
    assert_eq!(
        timed_out.take_sweep(),
        None,
        "a timed-out drain sweeps nothing"
    );

    let mut whole = CatchUp::new();
    whole.previewed(10);
    whole.drain_over(10);
    through(&mut whole);
    assert_eq!(whole.take_sweep(), Some(7));

    let mut no_preview = CatchUp::new();
    no_preview.drain_over(0);
    through(&mut no_preview);
    assert_eq!(no_preview.take_sweep(), Some(7), "nothing to compare with");
}

/// Fix F4: the facts the actor reads to tell a new connection from the same one. A reset forgets them.
#[test]
fn the_machine_remembers_the_end_of_a_drain_and_a_connected_until_it_is_reset() {
    let mut m = CatchUp::new();
    assert!((!m.drain_ended()) && !m.has_connected());
    m.connected();
    assert!(m.has_connected());
    m.interrupted();
    assert!(m.drain_ended(), "an interruption is an end too");
    m.reset();
    assert!((!m.drain_ended()) && !m.has_connected());
    m.drain_over(0);
    assert!(m.drain_ended());
}
