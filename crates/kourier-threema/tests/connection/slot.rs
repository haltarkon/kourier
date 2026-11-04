//! N5: the slot state. The first `ServerInfo` writes `existing` before anything else of that
//! connection (before `Syncing`), and a restarted actor never says `new` again; the crash window of
//! critic L13 (the slot exists, the file says `new`: a kill between a `ServerInfo` and its write)
//! costs ONE retry at once with `existing`; a second 4115, or a 4115 with `existing` expected, ends
//! the link.

use kourier_backend_api::Command;
use kourier_model::{ConnState, LogoutCause};
use kourier_threema::store::SlotState;
use libthreema::protobuf::d2m::DeviceSlotState;

use super::harness::{self, H, scene};
use crate::support;
use crate::support::mock::START_MS;

const SLOT_MISMATCH: &str = "4115 device slot state mismatch";

fn online(s: &ConnState) -> bool {
    matches!(s, ConnState::Online { .. })
}

fn logged_out(s: &ConnState) -> bool {
    matches!(s, ConnState::LoggedOut { .. })
}

/// The expected slot state of every `ClientHello` the fake saw, in order.
fn expected(h: &H) -> Vec<Option<DeviceSlotState>> {
    let m = h.s.world.mediator();
    h.net()
        .opened()
        .into_iter()
        .map(|conn| m.hello(conn).map(|hello| hello.expected))
        .collect()
}

/// A world in which this device's slot EXISTS at the mediator (a first actor connected with `new`
/// and shut down; PERSISTENT keeps the slot), then a NEW file that says `new`: the exact state a
/// kill between a `ServerInfo` and its write leaves (brief §2.13).
async fn seeded(seed: u64) -> H {
    let mut h = harness::start(scene(seed));
    h.send(Command::Connect).await;
    h.wait_state("Online", online).await;
    let mut s = h.stop().await;
    s.slot = SlotState::New;
    harness::start(s)
}

#[test]
fn the_first_server_info_writes_existing_before_syncing() {
    support::offline::run_child(
        module_path!(),
        "the_first_server_info_writes_existing_before_syncing_child",
    );
}

/// Dry held: at `Syncing` the file says `existing` already; a restarted actor's `ClientHello`
/// expects `existing`.
#[tokio::test]
async fn the_first_server_info_writes_existing_before_syncing_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(501);
    s.world.mediator().knobs(|k| k.hold_dry = true);
    let mut h = harness::start(s);
    assert_eq!(h.file_slot(), SlotState::New, "the seeded file");
    h.send(Command::Connect).await;
    h.wait_state("Syncing", |s| matches!(s, ConnState::Syncing { .. }))
        .await;
    assert_eq!(h.file_slot(), SlotState::Existing, "not written by Syncing");
    let mut h = h.restart().await;
    h.s.world.mediator().knobs(|k| k.hold_dry = false);
    h.send(Command::Connect).await;
    h.wait_state("Online", online).await;
    assert_eq!(
        expected(&h),
        vec![Some(DeviceSlotState::New), Some(DeviceSlotState::Existing)],
        "the hellos' expected states"
    );
    h.finish().await;
}

#[test]
fn a_known_slot_under_a_file_that_says_new_costs_one_retry() {
    support::offline::run_child(
        module_path!(),
        "a_known_slot_under_a_file_that_says_new_costs_one_retry_child",
    );
}

/// L13: 4115 for `new`, ONE retry at once (the clock not moved) with `existing`, `Online`, the file
/// `existing`.
#[tokio::test]
async fn a_known_slot_under_a_file_that_says_new_costs_one_retry_child() {
    if !support::offline::in_child() {
        return;
    }
    let mut h = seeded(502).await;
    assert_eq!(h.file_slot(), SlotState::New, "the seeded file");
    h.send(Command::Connect).await;
    h.wait_state("Online", online).await;
    assert_eq!(h.now(), START_MS, "the retry waited on the clock");
    let opened = h.net().opened();
    assert_eq!(opened.len(), 3, "the seeding, the 4115 and the retry");
    assert_eq!(h.s.world.mediator().closed(opened[1]), Some(4115));
    assert_eq!(
        expected(&h)[1..],
        [Some(DeviceSlotState::New), Some(DeviceSlotState::Existing)],
        "the hellos' expected states"
    );
    assert_eq!(
        h.file_slot(),
        SlotState::Existing,
        "the file after the retry"
    );
    h.finish().await;
}

#[test]
fn a_slot_dropped_before_the_retry_ends_the_link() {
    support::offline::run_child(
        module_path!(),
        "a_slot_dropped_before_the_retry_ends_the_link_child",
    );
}

/// L13 with the slot gone in between: 4115 twice, `LoggedOut { Unknown }`, `Exit::LoggedOut`,
/// exactly two connections of this actor.
#[tokio::test]
async fn a_slot_dropped_before_the_retry_ends_the_link_child() {
    if !support::offline::in_child() {
        return;
    }
    let mut h = seeded(503).await;
    h.s.world
        .mediator()
        .knobs(|k| k.drop_slot_at_next_4115 = true);
    h.send(Command::Connect).await;
    let state = h.wait_state("LoggedOut", logged_out).await;
    assert!(
        matches!(
            state,
            ConnState::LoggedOut {
                cause: LogoutCause::Unknown,
                ..
            }
        ),
        "not logged out with an unknown cause"
    );
    let opened = h.net().opened();
    assert_eq!(opened.len(), 3, "the seeding and two connections");
    let m = h.s.world.mediator();
    assert_eq!(m.closed(opened[1]), Some(4115));
    assert_eq!(m.closed(opened[2]), Some(4115));
    assert_eq!(
        expected(&h)[1..],
        [Some(DeviceSlotState::New), Some(DeviceSlotState::Existing)]
    );
    h.finish_logged_out(SLOT_MISMATCH).await;
}

#[test]
fn a_4115_with_existing_expected_ends_the_link_at_once() {
    support::offline::run_child(
        module_path!(),
        "a_4115_with_existing_expected_ends_the_link_at_once_child",
    );
}

/// The file says `existing`, the mediator knows no slot: no retry.
#[tokio::test]
async fn a_4115_with_existing_expected_ends_the_link_at_once_child() {
    if !support::offline::in_child() {
        return;
    }
    let mut s = scene(504);
    s.slot = SlotState::Existing;
    let mut h = harness::start(s);
    h.send(Command::Connect).await;
    h.wait_state("LoggedOut", logged_out).await;
    assert_eq!(
        h.net().opened().len(),
        1,
        "a retry after a 4115 for existing"
    );
    assert_eq!(expected(&h), vec![Some(DeviceSlotState::Existing)]);
    h.finish_logged_out(SLOT_MISMATCH).await;
}

#[test]
fn a_second_connection_task_of_the_same_actor_expects_existing() {
    support::offline::run_child(
        module_path!(),
        "a_second_connection_task_of_the_same_actor_expects_existing_child",
    );
}

/// P-M1 (protocol review F1, the reviewer's experiment): the slot state is the ACTOR's. Connect,
/// `Online`, `Disconnect`, `Connect`: two connections whose hellos expect `[New, Existing]`, no 4115
/// and no retry, the first closed by the client's 1000, the second still open, the file `existing`.
#[tokio::test]
async fn a_second_connection_task_of_the_same_actor_expects_existing_child() {
    if !support::offline::in_child() {
        return;
    }
    let mut h = harness::start(scene(831));
    assert_eq!(h.file_slot(), SlotState::New, "the seeded file");
    h.send(Command::Connect).await;
    h.wait_state("Online", online).await;
    h.send(Command::Disconnect).await;
    h.wait_state("Offline", |s| {
        *s == ConnState::Offline { retry_at_ms: None }
    })
    .await;
    assert_eq!(h.file_slot(), SlotState::Existing, "the first ServerInfo");
    h.send(Command::Connect).await;
    h.wait_state("Online again", online).await;
    let opened = h.net().opened();
    assert_eq!(opened.len(), 2, "a retry after a 4115");
    assert_eq!(
        expected(&h),
        vec![Some(DeviceSlotState::New), Some(DeviceSlotState::Existing)],
        "the hellos' expected states"
    );
    let m = h.s.world.mediator();
    assert_eq!(m.closed(opened[0]), Some(1000), "the Disconnect's close");
    assert_eq!(m.closed(opened[1]), None, "the second socket is open");
    assert_eq!(h.net().made(), 1, "the transports were made once");
    h.finish().await;
}
