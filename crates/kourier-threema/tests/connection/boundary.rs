//! TH5 A5b: THE CATCH-UP BOUNDARY through the real actor against the offline fake (`docs/architecture.md`
//! §4.5): the account stays `Syncing` until the mediator's `ReflectionQueueDry` came AND the core acked
//! every `core_outbox` row written before it; the messages of the backlog are `catch_up`, one that came
//! after Dry is not; a backlog that wrote no row goes `Online` at Dry; a `Disconnect` while the core's ack
//! is awaited drops the boundary (no `Online` after it, and the next socket waits for its own Dry). The
//! core side is the test: it reads the actor's envelopes and HOLDS their acks where a test needs it. Each
//! test is a parent/child `unshare -rn` pair like its neighbours; waits are wall bounds.

use std::time::Duration;

use kourier_backend_api::{Ack, BackendEvent, Command, IncomingMessage};
use kourier_model::{Body, ConnState};

use super::harness::{self, H, scene};
use super::reflect::{PEER, PHONE, connect, online, phone_of, wait_acks};
use crate::support;
use crate::support::mock::START_MS;
use crate::support::mock::body;
use crate::support::mock::envelope::{Author, Msg, Reflect as _};
use crate::support::mock::phone::Phone;

/// The upper bound of a wait (never a measure).
const WALL: Duration = Duration::from_secs(10);

fn syncing(s: &ConnState) -> bool {
    matches!(s, ConnState::Syncing { .. })
}

fn offline(s: &ConnState) -> bool {
    matches!(s, ConnState::Offline { .. })
}

/// The phone reflects `n` texts of `PEER` (CSP nonces and ids `[first..]`), queued for the account's
/// device while it is away.
fn reflect_texts(h: &H, phone: &mut Phone, first: u8, n: u8, text: &str) {
    let author = Author::new(PHONE);
    h.net().act(|_| {
        for k in first..first + n {
            phone.reflect_envelope(&author.incoming(
                PEER,
                &Msg::text([k; 8], START_MS + u64::from(k), text).nonce([k; 24]),
            ));
        }
    });
}

/// The core side HOLDING its acks: reads envelopes until `n` messages came (nothing but messages may
/// come); the messages and the held acks, in order.
async fn held(h: &mut H, n: usize) -> (Vec<IncomingMessage>, Vec<Ack>) {
    let (mut got, mut acks) = (Vec::new(), Vec::new());
    while got.len() < n {
        let envelope = h.envelope().await;
        acks.push(envelope.ack.expect("a durable envelope"));
        for event in envelope.events {
            match event {
                BackendEvent::Message(m) => got.push(m),
                _ => panic!("the actor shipped an event that is no message"),
            }
        }
    }
    assert_eq!(got.len(), n, "more messages than expected");
    (got, acks)
}

/// Away, then a backlog of three texts: `Connect`.
async fn away_with_a_backlog(h: &mut H, phone: &mut Phone) {
    connect(h).await;
    h.send(Command::Disconnect).await;
    h.wait_state("Offline", offline).await;
    reflect_texts(h, phone, 1, 3, "synthetic backlog");
    h.send(Command::Connect).await;
}

#[test]
fn a_backlog_keeps_syncing_until_the_core_acked_its_last_row() {
    support::offline::run_child(
        module_path!(),
        "a_backlog_keeps_syncing_until_the_core_acked_its_last_row_child",
    );
}

/// Three texts queued while the device was away, then Dry: the login-ack gives `Syncing` (Dry came before
/// it on the socket, so the boundary is known by then), and it STAYS `Syncing` while the core holds the
/// ack of the backlog's rows, after the mediator got its three reflected-acks too; the three are
/// `catch_up`; `Online` comes once the core acked the last one.
#[tokio::test]
async fn a_backlog_keeps_syncing_until_the_core_acked_its_last_row_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(651).with_contacts(&[PEER]);
    let mut phone = phone_of(&s);
    let mut h = harness::start(s);
    away_with_a_backlog(&mut h, &mut phone).await;
    h.wait_state("Syncing", syncing).await;
    let (got, mut acks) = held(&mut h, 3).await;
    assert!(
        got.iter()
            .all(|m| m.catch_up && m.body == Body::text("synthetic backlog")),
        "a message of the backlog is not catch-up"
    );
    wait_acks(&h, 3).await;
    let last = acks.pop().expect("an ack");
    for ack in acks {
        ack.complete(Ok(()));
    }
    h.sync().await;
    assert!(
        syncing(&h.state()),
        "not Syncing while the core holds the ack of the backlog's last row"
    );
    last.complete(Ok(()));
    h.wait_state("Online", online).await;
    h.finish().await;
}

#[test]
fn a_text_before_dry_is_catch_up_and_one_after_it_is_not() {
    support::offline::run_child(
        module_path!(),
        "a_text_before_dry_is_catch_up_and_one_after_it_is_not_child",
    );
}

/// With Dry held, a text that reaches the logged-in device is still part of the catch-up (`catch_up`), and
/// the account stays `Syncing` after the core acked it; Dry then goes `Online` at once (its boundary is
/// that row, already acked); a text after Dry is live (`catch_up` false).
#[tokio::test]
async fn a_text_before_dry_is_catch_up_and_one_after_it_is_not_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(652).with_contacts(&[PEER]);
    let mut phone = phone_of(&s);
    s.world.mediator().knobs(|k| k.hold_dry = true);
    let mut h = harness::start(s);
    h.send(Command::Connect).await;
    h.wait_state("Syncing", syncing).await;
    reflect_texts(&h, &mut phone, 1, 1, "synthetic before");
    let before = h.messages(1).await;
    assert!(before[0].catch_up, "a message before Dry is not catch-up");
    h.sync().await;
    assert!(syncing(&h.state()), "not Syncing before Dry");
    let conn = h.conn();
    assert!(h.net().act(|m| m.release_dry(conn)), "no Dry was held");
    h.wait_state("Online", online).await;
    reflect_texts(&h, &mut phone, 2, 1, "synthetic after");
    let after = h.messages(1).await;
    assert_eq!(after[0].body, Body::text("synthetic after"));
    assert!(!after[0].catch_up, "a message after Dry is catch-up");
    h.finish().await;
}

#[test]
fn a_backlog_without_rows_goes_online_at_dry() {
    support::offline::run_child(
        module_path!(),
        "a_backlog_without_rows_goes_online_at_dry_child",
    );
}

/// A backlog that writes no `core_outbox` row (a poll vote: DEFERRED, a `deferred` row only): with Dry held
/// the account is `Syncing`, Dry gives `Online` at once (nothing to wait for), and the core got nothing.
#[tokio::test]
async fn a_backlog_without_rows_goes_online_at_dry_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(653).with_contacts(&[PEER]);
    let mut phone = phone_of(&s);
    let mut h = harness::start(s);
    connect(&mut h).await;
    h.send(Command::Disconnect).await;
    h.wait_state("Offline", offline).await;
    h.net().act(|_| {
        phone.reflect_envelope(&Author::new(PHONE).incoming(
            PEER,
            &Msg::raw([1; 8], START_MS, body::ty::POLL_VOTE, body::filler(24, 1)).nonce([1; 24]),
        ));
    });
    h.s.world.mediator().knobs(|k| k.hold_dry = true);
    h.send(Command::Connect).await;
    h.wait_state("Syncing", syncing).await;
    wait_acks(&h, 1).await;
    let conn = h.conn();
    assert!(h.net().act(|m| m.release_dry(conn)), "no Dry was held");
    h.wait_state("Online", online).await;
    h.sync().await;
    h.nothing_for_the_core();
    assert_eq!(h.file_rows("deferred"), 1, "the vote is not deferred");
    h.finish().await;
}

#[test]
fn a_disconnect_while_the_core_acks_drops_the_boundary() {
    support::offline::run_child(
        module_path!(),
        "a_disconnect_while_the_core_acks_drops_the_boundary_child",
    );
}

/// A backlog of three texts, Dry, the core holding the acks: `Disconnect` (`Offline`), then the core acks
/// all three and the forwarder settles them: the account stays `Offline` (the boundary went with the
/// socket). The next socket starts without a boundary: with its Dry held it is `Syncing` although every
/// row is acked, and `Online` only after its own Dry.
#[tokio::test]
async fn a_disconnect_while_the_core_acks_drops_the_boundary_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(654).with_contacts(&[PEER]);
    let mut phone = phone_of(&s);
    let mut h = harness::start(s);
    away_with_a_backlog(&mut h, &mut phone).await;
    h.wait_state("Syncing", syncing).await;
    let (got, acks) = held(&mut h, 3).await;
    assert!(
        got.iter().all(|m| m.catch_up),
        "a backlog message is not catch-up"
    );
    wait_acks(&h, 3).await;
    h.send(Command::Disconnect).await;
    h.wait_state("Offline", offline).await;
    for ack in acks {
        ack.complete(Ok(()));
    }
    // The forwarder deletes an acked row in a task of its own: polled (a wall bound), as `J::wait_rows`.
    let settled = tokio::time::timeout(WALL, async {
        while h.file_rows("core_outbox") != 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    assert!(
        settled.is_ok(),
        "the forwarder did not settle the acked rows"
    );
    h.sync().await;
    assert!(
        offline(&h.state()),
        "the core's ack after a Disconnect changed the state: {:?}",
        h.state()
    );
    h.s.world.mediator().knobs(|k| k.hold_dry = true);
    h.send(Command::Connect).await;
    h.wait_state("Syncing", syncing).await;
    h.sync().await;
    assert!(
        syncing(&h.state()),
        "a new socket went Online before its own Dry"
    );
    let conn = h.conn();
    assert!(h.net().act(|m| m.release_dry(conn)), "no Dry was held");
    h.wait_state("Online", online).await;
    h.finish().await;
}
