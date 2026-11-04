//! The join's REFUSALS and CANCELS on the actor (TH4 J5b; brief §2.7, decisions B1, E2, E3, C12, M3):
//! an ID the profile has already, an ID not on a non-empty allow-list, a relink of another ID and a bad
//! item in `EssentialData` each fail at `linking` with NOTHING written (no identity row, every table of
//! the file empty), the mediator never hearing of the device (no ClientHello, its slots unchanged) and
//! the phone told 4100. A relink of the SAME ID passes the rules and joins. A `CancelLogin` after the
//! ClientHello (M3), one before the core applied row 1 (S3) and one after `Registered` (before the core's ack), and a
//! join that FAILS after the ClientHello (no `ServerInfo` in 20 s), pin what the file, the mediator and the core's
//! following `Logout` say: the device stays on the mediator until that `Logout`, which drops it (RP-b) on a D2M-only
//! socket of its own that expects EXISTING, and reports `Removed`. A 4115 on a later reconnect after `Done` is TH3's
//! path (`LoggedOut`, `Exit::LoggedOut`).

use std::time::Duration;

use kourier_backend_api::{BackendEvent, Command, Exit};
use kourier_model::{ConnState, DeviceLogout, ErrorKind, LogoutCause};
use kourier_threema::store::SlotState;
use libthreema::protobuf::d2m::DeviceSlotState;

use super::harness::{self, GROUP, J, Setup};
use crate::support;
use crate::support::mock::d2m::ConnId;
use crate::support::mock::essential::{Bad, Spec};
use crate::support::mock::joiner::JoinStep;
use crate::support::mock::{MockWorld, START_MS};

/// A wall BOUND of a wait for the fake (never a measure).
const WALL: Duration = Duration::from_secs(60);
/// Another synthetic Threema ID (a substring of no other test value).
const OTHER: &str = "KOURIER2";
/// Every table a join writes (schema 1 and 2) and the outbox to the core.
const TABLES: [&str; 11] = [
    "identity",
    "contact",
    "grp",
    "dist_list",
    "own_profile",
    "settings",
    "held_blob",
    "csp_nonce",
    "d2d_nonce",
    "join_ready",
    "core_outbox",
];
/// TH3's log-safe reason of a 4115.
const SLOT_MISMATCH: &str = "4115 device slot state mismatch";
/// TH3's handshake deadline of the join's socket (no `ServerInfo` in time), ms.
const HANDSHAKE_WITHIN: u64 = 20_000;

/// The fake's trace lines of `kind` the mediator got from a client.
fn mediator_got(world: &MockWorld, kind: &str) -> usize {
    world
        .trace()
        .lines()
        .iter()
        .filter(|l| {
            let t: Vec<&str> = l.split(' ').collect();
            t.get(2) == Some(&"in") && t.get(3) == Some(&kind)
        })
        .count()
}

/// The device ids the mediator holds a slot for in the joined device group.
fn slots(j: &J) -> Vec<u64> {
    j.world.mediator().slots(&j.keys.path_public)
}

/// RP-b, after the core's `Logout`: the drop went on ONE socket of its own after the join's (its ClientHello expected
/// EXISTING, so no slot could be made), named zone 1's `device`, and the mediator holds no slot for it.
fn dropped_on_a_socket_of_its_own(j: &J, device: u64) {
    let opened = j.net().opened();
    assert_eq!(
        opened.len(),
        2,
        "not the join's socket and one of the drop's own"
    );
    let m = j.world.mediator();
    assert_eq!(
        m.hello(opened[1]).map(|hello| hello.expected),
        Some(DeviceSlotState::Existing),
        "the drop's socket did not expect EXISTING"
    );
    let drops: Vec<(ConnId, u64)> = m.drops().iter().map(|d| (d.conn, d.device)).collect();
    assert_eq!(
        drops,
        vec![(opened[1], device)],
        "the drop did not name zone 1's device on its own socket"
    );
    assert!(
        slots(j).is_empty(),
        "the device's slot stayed on the mediator"
    );
}

/// A refusal before zone 1: the login failed at `linking` (txn 4) with `kind`; the file has no identity
/// row and every table is empty; the mediator got no ClientHello and its slots are what they were
/// (`before`); the phone saw 4100 (and the relay reflected it); no envelope follows.
async fn refused_with_nothing(j: &mut J, kind: &ErrorKind, before: &[u64]) {
    j.expect_step("linking", 3).await;
    j.expect_failed("linking", 4, kind).await;
    j.no_more_steps().await;
    assert!(!j.identity_exists(), "an identity row was written");
    for table in TABLES {
        assert_eq!(j.file_rows(table), 0, "the table {table} is not empty");
    }
    assert_eq!(
        mediator_got(&j.world, "client-hello"),
        0,
        "the mediator got a ClientHello"
    );
    assert_eq!(slots(j), before, "the mediator's slots changed");
    j.only_the_rendezvous();
    assert_eq!(j.joiner.closed(), Some(4100), "the phone did not see 4100");
    assert_eq!(j.rendezvous_closed(), Some(4100), "not closed 4100");
    assert!(
        j.joiner.ulp_received().is_empty(),
        "the phone got a frame of the join"
    );
}

/// Waits (a wall BOUND) until the connector opened the join's mediator socket and the mediator took
/// its ClientHello (the device registered); that connection.
async fn hello_taken(j: &J) -> ConnId {
    let net = j.net();
    assert!(
        net.wait(
            || net
                .opened()
                .first()
                .is_some_and(|c| j.world.mediator().hello(*c).is_some()),
            WALL
        )
        .await,
        "the mediator never took the join's ClientHello"
    );
    net.opened()[0]
}

#[test]
fn a_second_join_of_the_same_id_is_already_added_and_leaves_nothing() {
    support::offline::run_child(
        module_path!(),
        "a_second_join_of_the_same_id_is_already_added_and_leaves_nothing_child",
    );
}

/// brief §2.7 step 3: `taken` holds `threema:<ID>` (another account of the profile, active and logged
/// out): `Failed { AlreadyAdded { logged_out } }` at `linking`, the flag as `taken` says; nothing
/// written, no ClientHello, the mediator's slots unchanged, the phone saw 4100.
#[tokio::test]
async fn a_second_join_of_the_same_id_is_already_added_and_leaves_nothing_child() {
    if !support::offline::in_child() {
        return;
    }
    for (seed, logged_out) in [(511, false), (512, true)] {
        let mut j = harness::start(Setup {
            taken: true,
            taken_logged_out: logged_out,
            ..Setup::new(seed)
        });
        let before = slots(&j);
        j.nominated().await;
        refused_with_nothing(&mut j, &ErrorKind::AlreadyAdded { logged_out }, &before).await;
        j.finish().await;
    }
}

#[test]
fn an_id_not_on_a_non_empty_list_is_refused_at_linking() {
    support::offline::run_child(
        module_path!(),
        "an_id_not_on_a_non_empty_list_is_refused_at_linking_child",
    );
}

/// M6, E2: the parts' allow-list names another ID only: the login gets past B1's gate (a code is
/// shown) and fails `NotTheExpectedAccount` at `linking` once the phone sent the ID; nothing written,
/// no ClientHello, the phone saw 4100. (The EMPTY list fails at `qr`: J3's test.)
#[tokio::test]
async fn an_id_not_on_a_non_empty_list_is_refused_at_linking_child() {
    if !support::offline::in_child() {
        return;
    }
    let mut j = harness::start(Setup {
        listed: Some(&[OTHER]),
        ..Setup::new(513)
    });
    let before = slots(&j);
    j.nominated().await;
    refused_with_nothing(&mut j, &ErrorKind::NotTheExpectedAccount, &before).await;
    j.finish().await;
}

#[test]
fn a_relink_of_another_id_is_invalid_input_and_leaves_nothing() {
    support::offline::run_child(
        module_path!(),
        "a_relink_of_another_id_is_invalid_input_and_leaves_nothing_child",
    );
}

/// brief §2.7 step 2: a relink of the account whose primary own ID is another one (both IDs on the
/// list): `Failed { InvalidInput }` at `linking`, nothing written, no ClientHello, the phone saw 4100.
#[tokio::test]
async fn a_relink_of_another_id_is_invalid_input_and_leaves_nothing_child() {
    if !support::offline::in_child() {
        return;
    }
    let mut j = harness::start(Setup {
        listed: Some(&[GROUP, OTHER]),
        relink: Some(OTHER),
        ..Setup::new(514)
    });
    let before = slots(&j);
    j.nominated().await;
    refused_with_nothing(&mut j, &ErrorKind::InvalidInput, &before).await;
    j.finish().await;
}

#[test]
fn a_relink_of_the_same_id_passes_the_rules_and_joins() {
    support::offline::run_child(
        module_path!(),
        "a_relink_of_the_same_id_passes_the_rules_and_joins_child",
    );
}

/// A relink whose primary own ID is the joined one passes the rules: into the core's fresh file it is
/// the WHOLE join (one ClientHello expecting NEW, zone 2 with `AccountReady` first, `Done` after the
/// acks, the slot `existing`, the rendezvous closed 1000), as a first link.
#[tokio::test]
async fn a_relink_of_the_same_id_passes_the_rules_and_joins_child() {
    if !support::offline::in_child() {
        return;
    }
    let mut j = harness::start(Setup {
        relink: Some(GROUP),
        ..Setup::new(515)
    });
    j.nominated().await;
    j.expect_step("linking", 3).await;
    let got = j.ack_until_done(Vec::new(), 4).await;
    assert!(
        matches!(got.first(), Some(BackendEvent::AccountReady { .. })),
        "the core side's first event is not AccountReady"
    );
    assert!(
        got == j.file_states(),
        "the core side got other events than zone 2 wrote"
    );
    assert_eq!(
        mediator_got(&j.world, "client-hello"),
        1,
        "not exactly one ClientHello"
    );
    let identity = j.file_identity().expect("the relink wrote the identity");
    assert_eq!(
        identity.slot_state,
        SlotState::Existing,
        "the slot is not existing"
    );
    assert_eq!(
        j.world
            .mediator()
            .hello(j.net().opened()[0])
            .map(|h| h.expected),
        Some(DeviceSlotState::New),
        "the relink's ClientHello did not expect NEW"
    );
    assert_eq!(j.rendezvous_closed(), Some(1000), "not closed 1000");
    j.no_more_steps().await;
    j.finish().await;
}

#[test]
fn a_bad_item_in_the_data_is_unsupported_and_leaves_nothing() {
    support::offline::run_child(
        module_path!(),
        "a_bad_item_in_the_data_is_unsupported_and_leaves_nothing_child",
    );
}

/// C12, E3: a group member who is not a contact, and Work credentials, in `EssentialData`: the join is
/// refused `Unsupported` at `linking` before anything is written; no ClientHello, the phone saw 4100.
#[tokio::test]
async fn a_bad_item_in_the_data_is_unsupported_and_leaves_nothing_child() {
    if !support::offline::in_child() {
        return;
    }
    for (seed, bad) in [(516, Bad::MemberNotAContact), (517, Bad::WorkCredentials)] {
        let mut setup = Setup::new(seed);
        setup.spec = Spec {
            bad: vec![bad],
            ..setup.spec
        };
        let mut j = harness::start(setup);
        let before = slots(&j);
        j.nominated().await;
        refused_with_nothing(&mut j, &ErrorKind::Unsupported, &before).await;
        j.finish().await;
    }
}

#[test]
fn a_cancel_after_the_client_hello_stops_the_join_and_its_logout_drops_the_device() {
    support::offline::run_child(
        module_path!(),
        "a_cancel_after_the_client_hello_stops_the_join_and_its_logout_drops_the_device_child",
    );
}

/// M3, closed by RP-b (architecture §4.3 step 2): zone 1 is written and the mediator REGISTERED the device (its
/// ClientHello taken, the `ServerInfo` withheld); `CancelLogin` stops the join (the rendezvous closed 1000) and its
/// connection (closed 1000 by Kourier): no zone 2, nothing at the core side, no `Registered`. What stays until the
/// core's `Logout`: the file's identity row (slot `new`) and its data, and the device's slot on the mediator. The
/// `Logout` drops the device from zone 1's identity on a socket of its own and reports `Removed` with the label.
#[tokio::test]
async fn a_cancel_after_the_client_hello_stops_the_join_and_its_logout_drops_the_device_child() {
    if !support::offline::in_child() {
        return;
    }
    let mut j = harness::start(Setup::new(518));
    j.world.mediator().knobs(|k| k.withhold_server_info = true);
    j.nominated().await;
    j.expect_step("linking", 3).await;
    let conn = hello_taken(&j).await;
    j.sync().await;
    let device = j
        .file_identity()
        .expect("zone 1 wrote the identity")
        .d2m_device_id;
    assert_eq!(
        slots(&j),
        vec![device],
        "the mediator did not register the device"
    );
    j.send(Command::CancelLogin).await;
    j.sync().await;
    assert_eq!(
        j.world.mediator().closed(conn),
        Some(1000),
        "the join's connection was not closed 1000 at the cancel"
    );
    j.no_more_steps().await;
    assert_eq!(
        j.file_rows("core_outbox"),
        0,
        "zone 2 was written after the cancel"
    );
    let identity = j.file_identity().expect("the identity row stays");
    assert!(
        identity.slot_state == SlotState::New && identity.device_label == j.label,
        "the identity row is not zone 1's"
    );
    assert_eq!(j.file_rows("contact"), 2, "zone 1's contacts did not stay");
    assert_eq!(
        slots(&j),
        vec![device],
        "the device's slot did not stay on the mediator"
    );
    assert_eq!(
        mediator_got(&j.world, "client-hello"),
        1,
        "not exactly one ClientHello"
    );
    assert_eq!(j.net().opened().len(), 1, "a second mediator connection");
    assert!(
        j.joiner.ulp_received().is_empty(),
        "the phone got Registered"
    );
    assert_eq!(j.rendezvous_closed(), Some(1000), "not closed 1000");
    assert_eq!(j.joiner.closed(), Some(1000), "the phone did not see 1000");
    let label = j.label.clone();
    j.logout(DeviceLogout::Removed, Some(&label)).await;
    dropped_on_a_socket_of_its_own(&j, device);
    j.world.violations().assert_empty();
}

#[test]
fn a_join_that_fails_after_the_client_hello_drops_its_device_at_the_cores_logout() {
    support::offline::run_child(
        module_path!(),
        "a_join_that_fails_after_the_client_hello_drops_its_device_at_the_cores_logout_child",
    );
}

/// RP-b, the join's FAILURE end (critic C-H2): the mediator registered the device (its ClientHello taken) but no
/// `ServerInfo` came within 20 s: `Failed { Transient }` at `linking`, the join's socket closed 1000 by Kourier, the
/// slot left on the mediator. The core's `Logout` that follows (it sends one after every failed login, before it
/// deletes the `Linking` directory) drops the device from zone 1's identity: `Removed` with the label, the slot gone.
#[tokio::test]
async fn a_join_that_fails_after_the_client_hello_drops_its_device_at_the_cores_logout_child() {
    if !support::offline::in_child() {
        return;
    }
    let mut j = harness::start(Setup::new(521));
    j.world.mediator().knobs(|k| k.withhold_server_info = true);
    j.nominated().await;
    j.expect_step("linking", 3).await;
    let conn = hello_taken(&j).await;
    j.advance_to(START_MS + HANDSHAKE_WITHIN).await;
    j.expect_failed("linking", 4, &ErrorKind::Transient { retry_after: None })
        .await;
    assert_eq!(
        j.world.mediator().closed(conn),
        Some(1000),
        "the join's socket was not closed 1000"
    );
    let device = j
        .file_identity()
        .expect("zone 1 wrote the identity")
        .d2m_device_id;
    assert_eq!(
        slots(&j),
        vec![device],
        "the mediator did not keep the registered device"
    );
    let label = j.label.clone();
    j.logout(DeviceLogout::Removed, Some(&label)).await;
    dropped_on_a_socket_of_its_own(&j, device);
    j.world.violations().assert_empty();
}

#[test]
fn a_cancel_after_registered_before_the_ack_never_says_done() {
    support::offline::run_child(
        module_path!(),
        "a_cancel_after_registered_before_the_ack_never_says_done_child",
    );
}

/// AS BUILT: zone 2 is written, the core applied row 1 and `Registered` went to the phone (the rendezvous
/// closed 1000), the core side holds the ack of the LAST row (the rows come one by one: the first batch was
/// refused for good); `CancelLogin` stops the connection (closed 1000 by Kourier) and drops the wait for
/// the ack, so `Done` never comes, even once the core side acks the last row. The file keeps the identity
/// (slot `existing`), the mediator the slot, until the core's `Logout` (the core sends it after every cancel,
/// architecture §4.3 step 2): it drops the device (RP-b) and reports `Removed` with the label.
#[tokio::test]
async fn a_cancel_after_registered_before_the_ack_never_says_done_child() {
    if !support::offline::in_child() {
        return;
    }
    let mut j = harness::start(Setup::new(519));
    j.nominated().await;
    j.expect_step("linking", 3).await;
    let (mut got, ack_one) = j.row_one().await;
    ack_one.complete(Ok(()));
    j.wait_joiner(JoinStep::Closed).await;
    assert_eq!(
        j.joiner.ulp_received().len(),
        1,
        "the phone did not get Registered"
    );
    let (last, ack_last) = j.durable().await;
    let conn = j.net().opened()[0];
    j.send(Command::CancelLogin).await;
    j.sync().await;
    assert_eq!(
        j.world.mediator().closed(conn),
        Some(1000),
        "the connection was not closed 1000 at the cancel"
    );
    assert!(j.file_rows("core_outbox") > 0, "zone 2's last row went");
    j.no_more_steps().await;
    ack_last.complete(Ok(()));
    got.extend(last);
    assert!(
        got == j.file_states(),
        "the core side got other events than zone 2 wrote"
    );
    j.no_more_steps().await;
    let identity = j.file_identity().expect("the identity row stays");
    assert_eq!(
        identity.slot_state,
        SlotState::Existing,
        "the slot is not existing"
    );
    assert_eq!(
        slots(&j),
        vec![identity.d2m_device_id],
        "the device's slot did not stay on the mediator"
    );
    assert_eq!(j.rendezvous_closed(), Some(1000), "not closed 1000");
    let label = j.label.clone();
    j.logout(DeviceLogout::Removed, Some(&label)).await;
    dropped_on_a_socket_of_its_own(&j, identity.d2m_device_id);
    j.world.violations().assert_empty();
}

#[test]
fn a_logout_counts_its_eight_seconds_from_the_logout_and_not_from_the_end_of_a_stalled_join_close()
{
    support::offline::run_child(
        module_path!(),
        "a_logout_counts_its_eight_seconds_from_the_logout_and_not_from_the_end_of_a_stalled_join_close_child",
    );
}

/// RP-b (P-L1): a `Logout` that finds the join still open (zone 2 shipped, the core holds the ack of row 1) stops it first,
/// and the relay does not answer the close: the actor gives it up after one second of its clock. The drop's 8 s counts
/// from the LOGOUT (taken before that stop), not from its end, and every wait ends one closing second before: with the
/// mediator silent on the drop, the live socket's wait ends at +7 s (not +9 s), the report is `NotReached` and comes by
/// +8 s of the actor's clock, inside the core's 10 s.
#[tokio::test]
async fn a_logout_counts_its_eight_seconds_from_the_logout_and_not_from_the_end_of_a_stalled_join_close_child()
 {
    if !support::offline::in_child() {
        return;
    }
    let mut j = harness::start(Setup::new(522));
    j.world.mediator().knobs(|k| k.withhold_drop_ack = true);
    j.nominated().await;
    j.expect_step("linking", 3).await;
    let (first, ack) = j.durable().await;
    assert!(
        matches!(first.first(), Some(BackendEvent::AccountReady { .. })),
        "the core side's first event is not AccountReady"
    );
    let device = j
        .file_identity()
        .expect("zone 1 wrote the identity")
        .d2m_device_id;
    let at = j.now();
    j.net().stall_relay_closes(true);
    j.send(Command::Logout).await;
    // The join's close pends: the actor gives it up one second later and goes on to the drop.
    j.advance_to(at + 1_000).await;
    // The drop's deadline was taken at `at`: the wait ends at +7 s; a deadline taken after the stop would be +9 s.
    j.advance_to(at + 7_000).await;
    let label = j.label.clone();
    j.reported(DeviceLogout::NotReached, Some(&label)).await;
    assert!(
        j.now() <= at + 8_000,
        "the report came after 8 s of the actor's clock"
    );
    let conn = j.net().opened()[0];
    let drops: Vec<(ConnId, u64)> = j
        .world
        .mediator()
        .drops()
        .iter()
        .map(|d| (d.conn, d.device))
        .collect();
    assert_eq!(drops, vec![(conn, device)], "the drop on the live socket");
    assert_eq!(slots(&j), vec![device], "the slot went without an answer");
    ack.complete(Ok(()));
    j.world.violations().assert_empty();
}

#[test]
fn a_cancel_before_the_core_applied_the_first_row_never_registers() {
    support::offline::run_child(
        module_path!(),
        "a_cancel_before_the_core_applied_the_first_row_never_registers_child",
    );
}

/// S3: zone 2 is written and shipped, the core side holds the ack of row 1; `CancelLogin` closes the
/// rendezvous 1000 (the phone sees it) with NO `Registered` ever sent, and stops the connection; the
/// ack that comes later moves nothing: no `Done`, no frame. The file keeps the identity; the core's
/// `Logout` drops the device (RP-b) and reports `Removed` with the label.
#[tokio::test]
async fn a_cancel_before_the_core_applied_the_first_row_never_registers_child() {
    if !support::offline::in_child() {
        return;
    }
    let mut j = harness::start(Setup::new(520));
    j.nominated().await;
    j.expect_step("linking", 3).await;
    let (first, ack) = j.durable().await;
    assert!(
        matches!(first.first(), Some(BackendEvent::AccountReady { .. })),
        "the core side's first event is not AccountReady"
    );
    let conn = j.net().opened()[0];
    j.send(Command::CancelLogin).await;
    j.sync().await;
    assert_eq!(
        j.world.mediator().closed(conn),
        Some(1000),
        "the connection was not closed 1000 at the cancel"
    );
    assert_eq!(j.rendezvous_closed(), Some(1000), "not closed 1000");
    assert_eq!(j.joiner.closed(), Some(1000), "the phone did not see 1000");
    assert!(
        j.joiner.ulp_received().is_empty(),
        "the phone got Registered after a cancel"
    );
    ack.complete(Ok(()));
    j.no_more_steps().await;
    assert!(
        j.joiner.ulp_received().is_empty(),
        "the ack of row 1 made a Registered go out after the cancel"
    );
    let device = j
        .file_identity()
        .expect("the identity row went")
        .d2m_device_id;
    let label = j.label.clone();
    j.logout(DeviceLogout::Removed, Some(&label)).await;
    dropped_on_a_socket_of_its_own(&j, device);
    j.world.violations().assert_empty();
}

#[test]
fn a_4115_on_a_later_reconnect_after_done_logs_out() {
    support::offline::run_child(
        module_path!(),
        "a_4115_on_a_later_reconnect_after_done_logs_out_child",
    );
}

/// After `Done` the connection is TH3's: `Disconnect`, the slot dropped on the mediator meanwhile,
/// `Connect`: the second ClientHello expects EXISTING (the file and the actor say so), the mediator
/// closes 4115, no retry; `LoggedOut { Unknown }` and the actor ends `Exit::LoggedOut`.
#[tokio::test]
async fn a_4115_on_a_later_reconnect_after_done_logs_out_child() {
    if !support::offline::in_child() {
        return;
    }
    let mut j = harness::start(Setup::new(520));
    j.nominated().await;
    j.expect_step("linking", 3).await;
    j.ack_until_done(Vec::new(), 4).await;
    j.wait_state("Online", |s| matches!(s, ConnState::Online { .. }))
        .await;
    j.send(Command::Disconnect).await;
    j.sync().await;
    let device = j
        .file_identity()
        .expect("the joined identity")
        .d2m_device_id;
    assert!(
        j.world.mediator().drop_slot(&j.keys.path_public, device),
        "the device had no slot"
    );
    j.send(Command::Connect).await;
    j.wait_state("LoggedOut", |s| {
        matches!(
            s,
            ConnState::LoggedOut { reason, cause: LogoutCause::Unknown, .. } if reason == SLOT_MISMATCH
        )
    })
    .await;
    let opened = j.net().opened();
    assert_eq!(opened.len(), 2, "not the join's and one reconnect");
    let m = j.world.mediator();
    let expected: Vec<_> = opened
        .iter()
        .map(|c| m.hello(*c).map(|h| h.expected))
        .collect();
    assert_eq!(
        expected,
        vec![Some(DeviceSlotState::New), Some(DeviceSlotState::Existing)],
        "the hellos did not expect NEW, then EXISTING"
    );
    assert_eq!(
        m.closed(opened[1]),
        Some(4115),
        "the reconnect was not closed 4115"
    );
    j.finish_ended(Exit::LoggedOut {
        reason: SLOT_MISMATCH.to_owned(),
    })
    .await;
}
