//! TH5 A5a, STEP 0 (decision B1): reflections the mediator delivers to the joined device between the
//! join's first `ServerInfo` and zone 2 wait in memory and are applied right after zone 2 commits (the
//! fake's ack hook reads the file at every ack: the `join_ready` marker is there with the row); more than
//! 30 of them close the socket 1000 and come again on the next one, then are applied once each. A5a
//! writes no `core_outbox` row for a reflection (every one is deferred), so "after zone 2" is read from
//! the marker at the ack; nothing is quarantined.

use std::sync::{Arc, Mutex};

use kourier_backend_api::BackendEvent;
use kourier_db::rusqlite::Connection;
use kourier_model::ConnState;
use kourier_threema::store;
use libthreema::protobuf::d2d;

use super::harness::{self, J, Setup};
use crate::support;
use crate::support::key;
use crate::support::mock::START_MS;
use crate::support::mock::body;
use crate::support::mock::envelope::{Author, Msg, flags_for};
use crate::support::mock::essential::contact_id;

/// The reflecting phone's device id (a second device of the joined group).
const PHONE: u64 = 9;

/// An envelope of a DEFERRED kind (a poll vote, kept whole; a text from a sender the file does not know is
/// dropped since A6, decision M2; TH5b maps the reaction, the edit and the delete, so a vote stands in).
fn vote(n: u8) -> d2d::Envelope {
    Author::new(PHONE).incoming(
        "PEERAAAA",
        &Msg::raw([n; 8], START_MS, body::ty::POLL_VOTE, body::filler(24, n)).nonce([n; 24]),
    )
}

fn rows(conn: &Connection, table: &str) -> i64 {
    conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .expect("count the rows")
}

/// Queues `n` reflections for the next new slot (the fake's `reflect_during_join`) and installs the ack hook
/// that reads (join_ready, deferred) from the file at every ack.
fn reflect_during_join(j: &J, n: u8) -> Arc<Mutex<Vec<(i64, i64)>>> {
    let mut phone = j.world.phone(&j.dgk, &j.server_group, PHONE);
    for k in 1..=n {
        let envelope = vote(k);
        let sealed = phone.seal(&envelope);
        j.world
            .mediator()
            .reflect_during_join(flags_for(&envelope), sealed);
    }
    let seen: Arc<Mutex<Vec<(i64, i64)>>> = Arc::default();
    let (file, at_ack) = (j.db_file(), seen.clone());
    j.world.mediator().on_reflected_ack(move |_ack| {
        let db = store::open(&file, &key()).expect("open the file at the ack");
        let got = db
            .read(|c: &Connection| {
                Ok::<_, kourier_db::Error>((rows(c, "join_ready"), rows(c, "deferred")))
            })
            .expect("read at the ack");
        at_ack.lock().expect("the hook's list").push(got);
    });
    seen
}

/// The join up to `Done`, the core side acking every row.
async fn join(j: &mut J) {
    j.nominated().await;
    j.expect_step("linking", 3).await;
    let (first, ack) = j.durable().await;
    assert!(
        matches!(
            first.first(),
            Some(kourier_backend_api::BackendEvent::AccountReady { .. })
        ),
        "row 1 is not AccountReady"
    );
    ack.complete(Ok(()));
    j.ack_until_done(first, 4).await;
}

/// Waits (a wall bound) until the fake holds `n` reflected-acks; each named a known id.
async fn wait_acks(j: &J, n: usize) {
    let net = j.net();
    let m = j.world.mediator();
    assert!(
        net.wait(
            || m.reflected_acks().len() >= n,
            std::time::Duration::from_secs(30)
        )
        .await,
        "the fake never saw {n} reflected-acks (it saw {})",
        m.reflected_acks().len()
    );
    let acks = m.reflected_acks();
    assert_eq!(acks.len(), n, "more reflected-acks than expected");
    assert!(acks.iter().all(|a| a.known), "an ack named an unknown id");
}

#[test]
fn reflections_during_the_join_are_applied_after_zone_two() {
    support::offline::run_child(
        module_path!(),
        "reflections_during_the_join_are_applied_after_zone_two_child",
    );
}

/// B1: three reflections (deferred votes) between the join's `ServerInfo` and zone 2: the link ends `Done`; every ack
/// came with the marker in the file (zone 2 committed first) and the batch's rows; three deferred rows,
/// nothing quarantined, one mediator connection.
#[tokio::test]
async fn reflections_during_the_join_are_applied_after_zone_two_child() {
    if !support::offline::in_child() {
        return;
    }
    let mut j = harness::start(Setup::new(521));
    j.world.mediator().knobs(|k| k.auto_leader = false);
    let seen = reflect_during_join(&j, 3);
    join(&mut j).await;
    wait_acks(&j, 3).await;
    let at_acks = seen.lock().expect("the hook's list").clone();
    assert!(
        at_acks.len() == 3
            && at_acks
                .iter()
                .all(|(ready, rows)| *ready == 1 && *rows == 3),
        "a reflection was acked before zone 2, or before its own zone: {at_acks:?}"
    );
    assert_eq!(j.file_rows("deferred"), 3, "not three deferred rows");
    assert_eq!(
        j.file_rows("core_outbox_poison"),
        0,
        "a row was quarantined"
    );
    assert_eq!(j.net().opened().len(), 1, "not one mediator connection");
    j.finish().await;
}

#[test]
fn more_than_thirty_reflections_during_the_join_close_1000_and_come_again() {
    support::offline::run_child(
        module_path!(),
        "more_than_thirty_reflections_during_the_join_close_1000_and_come_again_child",
    );
}

/// B1's bound: 31 reflections before zone 2 are more than may wait: the first socket closes 1000, the
/// mediator delivers them again on the next socket (after zone 2), and each is applied and acked once.
/// S10 (round A): the actor's clock never moves in this test, so the second socket opens only because the
/// connection waits for the FILE to be ready (the one backoff step it would otherwise wait is 5 s of that
/// clock).
#[tokio::test]
async fn more_than_thirty_reflections_during_the_join_close_1000_and_come_again_child() {
    if !support::offline::in_child() {
        return;
    }
    let mut j = harness::start(Setup::new(522));
    j.world.mediator().knobs(|k| k.auto_leader = false);
    let seen = reflect_during_join(&j, 31);
    join(&mut j).await;
    wait_acks(&j, 31).await;
    let opened = j.net().opened();
    assert!(opened.len() >= 2, "the socket was not replaced");
    assert_eq!(
        j.world.mediator().closed(opened[0]),
        Some(1000),
        "the first socket was not closed 1000"
    );
    let at_acks = seen.lock().expect("the hook's list").clone();
    assert!(
        at_acks.iter().all(|(ready, _)| *ready == 1),
        "a reflection was acked before zone 2"
    );
    assert_eq!(j.file_rows("deferred"), 31, "not 31 deferred rows");
    assert_eq!(
        j.file_rows("core_outbox_poison"),
        0,
        "a row was quarantined"
    );
    j.finish().await;
}

#[test]
fn a_backlog_held_during_the_join_is_below_the_boundary() {
    support::offline::run_child(
        module_path!(),
        "a_backlog_held_during_the_join_is_below_the_boundary_child",
    );
}

/// TH5 A5b: three texts of a contact the join imports reach the joined device between its `ServerInfo` and
/// zone 2, and Dry right after them: the texts wait (step 0), are applied after zone 2 as `catch_up`, and Dry
/// is counted only then, so the boundary holds zone 2's rows AND the texts' rows: the account is not `Online`
/// while the core holds the ack of any of them (checked right before each ack), and is `Online` once the core
/// acked the last one.
#[tokio::test]
async fn a_backlog_held_during_the_join_is_below_the_boundary_child() {
    if !support::offline::in_child() {
        return;
    }
    let mut j = harness::start(Setup::new(523));
    j.world.mediator().knobs(|k| k.auto_leader = false);
    let mut phone = j.world.phone(&j.dgk, &j.server_group, PHONE);
    let sender = contact_id(0);
    for n in 1..=3u8 {
        let envelope = Author::new(PHONE).incoming(
            &sender,
            &Msg::text([n; 8], START_MS + u64::from(n), "synthetic held").nonce([n; 24]),
        );
        let sealed = phone.seal(&envelope);
        j.world
            .mediator()
            .reflect_during_join(flags_for(&envelope), sealed);
    }
    j.nominated().await;
    j.expect_step("linking", 3).await;
    let (mut messages, mut done) = (Vec::new(), false);
    while !(done && messages.len() == 3) {
        let envelope = j.envelope().await;
        let Some(ack) = envelope.ack else {
            assert!(
                matches!(envelope.events.as_slice(), [BackendEvent::Login(step)] if step.step_id == "done"),
                "an ephemeral envelope that is not the done step"
            );
            done = true;
            continue;
        };
        for event in envelope.events {
            if let BackendEvent::Message(m) = event {
                messages.push(m);
            }
        }
        assert!(
            !matches!(j.state(), ConnState::Online { .. }),
            "Online while the core holds the ack of a row below the boundary"
        );
        ack.complete(Ok(()));
    }
    assert!(
        messages.iter().all(|m| m.catch_up),
        "a message that waited for zone 2 is not catch-up"
    );
    j.wait_state("Online", |s| matches!(s, ConnState::Online { .. }))
        .await;
    j.wait_rows("core_outbox", 0).await;
    assert_eq!(
        j.file_rows("core_outbox_poison"),
        0,
        "a row was quarantined"
    );
    j.finish().await;
}
