//! The WHOLE JOIN on the actor (TH4 J5a, decisions H1, M1, C3, L8): `EssentialData` -> the checks ->
//! zone 1 -> the mediator with the join hook -> the first `ServerInfo` says `NEW` -> zone 2
//! (`AccountReady` first) -> `Registered` to the phone, the rendezvous closed 1000 -> `Done` only after
//! the core side acked. The phone's data is built from the TEST's own world (the harness's device
//! group), so the fake mediator and chat server know the joined device. The slot refusal on the
//! mediator's real 4115 path (no L13 retry), the 20 s deadline without `ServerInfo` and the clock rule
//! at the join's first `ServerInfo`.

use std::time::Duration;

use kourier_backend_api::{BackendEvent, Command};
use kourier_model::{ConnState, DeviceLogout, ErrorKind, UserAction};
use kourier_threema::store::SlotState;
use libthreema::protobuf::{d2d, d2d_join, d2m};
use prost::Message as _;
use tracing::Level;

use super::harness::{self, J, Setup};
use crate::support;
use crate::support::mock::crypto::open_nonce_ahead;
use crate::support::mock::d2m::HelloSeen;
use crate::support::mock::essential::Spec;
use crate::support::mock::joiner::JoinStep;
use crate::support::mock::{MockWorld, START_MS};
use crate::support::warns::Warns;

/// A wall BOUND of a wait for the fake (never a measure).
const WALL: Duration = Duration::from_secs(60);
/// TH3's handshake deadline: no `ServerInfo` within it fails the join.
const HANDSHAKE_WITHIN: u64 = 20_000;
/// The fake's `ServerInfo` time is this far ahead of both clocks in the clock test (over 20 min).
const SKEW_MS: i64 = 25 * 60_000;

/// An account of 3 contacts and 2 groups; the ID on the list.
fn setup(seed: u64) -> Setup {
    Setup {
        spec: Spec {
            contacts: 3,
            groups: 2,
            hashed_nonces: 2,
            profile_picture: false,
            bad: Vec::new(),
        },
        ..Setup::new(seed)
    }
}

/// The trace's tokens per line (`<n> <connection> <in|out> <kind> ...`).
fn tokens(world: &MockWorld) -> Vec<Vec<String>> {
    world
        .trace()
        .lines()
        .iter()
        .map(|l| l.split(' ').map(str::to_owned).collect())
        .collect()
}

/// How many `ClientHello`s the fake mediator got.
fn client_hellos(world: &MockWorld) -> usize {
    tokens(world)
        .iter()
        .filter(|t| {
            t.get(2).map(String::as_str) == Some("in")
                && t.get(3).map(String::as_str) == Some("client-hello")
        })
        .count()
}

/// Every ULP frame the phone got from Kourier is ONE `NdToEd { Registered }`.
fn the_phone_got_registered_once(j: &J) {
    let frames = j.joiner.ulp_received();
    assert_eq!(
        frames.len(),
        1,
        "the phone did not get exactly one frame of the join"
    );
    let message = d2d_join::NdToEd::decode(frames[0].as_slice()).expect("an NdToEd");
    assert!(
        matches!(
            message.content,
            Some(d2d_join::nd_to_ed::Content::Registered(_))
        ),
        "the phone's frame is not Registered"
    );
}

/// The fake's trace order: the RID's LAST rendezvous message (`Registered`) after the mediator's
/// `ServerInfo`.
fn registered_after_server_info(j: &J) {
    let lines = tokens(&j.world);
    let info = lines
        .iter()
        .position(|t| {
            t.get(2).map(String::as_str) == Some("out")
                && t.get(3).map(String::as_str) == Some("server-info")
        })
        .expect("the mediator sent ServerInfo");
    let side = j.side().0.to_string();
    let last = lines
        .iter()
        .rposition(|t| {
            t.get(1) == Some(&side)
                && t.get(2).map(String::as_str) == Some("in")
                && t.get(3).map(String::as_str) == Some("rendezvous-message")
        })
        .expect("Kourier sent on the rendezvous");
    assert!(
        info < last,
        "Registered went to the phone before the mediator's ServerInfo"
    );
}

/// The core side's first durable envelope starts with `AccountReady`.
fn account_ready_first(events: &[BackendEvent]) {
    assert!(
        matches!(events.first(), Some(BackendEvent::AccountReady { .. })),
        "the core side's first event is not AccountReady"
    );
}

/// Waits (a wall BOUND) until the connector opened the mediator socket of the join; that connection.
async fn mediator_opened(j: &J) -> crate::support::mock::d2m::ConnId {
    let net = j.net();
    assert!(
        net.wait(|| net.opened().len() == 1, WALL).await,
        "the join never connected to the mediator"
    );
    net.opened()[0]
}

#[test]
fn the_whole_join_registers_the_device_and_is_done_after_the_ack() {
    support::offline::run_child(
        module_path!(),
        "the_whole_join_registers_the_device_and_is_done_after_the_ack_child",
    );
}

/// `qr` -> `emoji` -> `linking`; zone 1 and ONE ClientHello (expected NEW, Kourier's label, the drawn
/// device id); `Registered` after the mediator's `ServerInfo` (the fake's trace); zone 2's rows reach the
/// core side, `AccountReady` first, equal to what the file states; `Done` LAST, after the acks; the
/// file holds 3 contacts, 2 groups and the slot `existing`; `Online { follower }` once Dry is released;
/// the rendezvous closed 1000.
#[tokio::test]
async fn the_whole_join_registers_the_device_and_is_done_after_the_ack_child() {
    if !support::offline::in_child() {
        return;
    }
    let mut j = harness::start(setup(501));
    j.world.mediator().knobs(|k| {
        k.auto_leader = false;
        k.hold_dry = true;
    });
    j.nominated().await;
    j.expect_step("linking", 3).await;
    let (first, ack) = j.durable().await;
    account_ready_first(&first);
    ack.complete(Ok(()));
    let got = j.ack_until_done(first, 4).await;
    assert!(
        got == j.file_states(),
        "the core side got other events than zone 2 wrote"
    );
    j.the_phone_got_registered_once_check();
    registered_after_server_info(&j);
    assert_eq!(
        j.rendezvous_closed(),
        Some(1000),
        "the rendezvous was not closed 1000"
    );
    assert_eq!(
        j.joiner.closed(),
        Some(1000),
        "the phone did not see the 1000"
    );
    let opened = j.net().opened();
    assert_eq!(opened.len(), 1, "not one mediator connection");
    assert_eq!(client_hellos(&j.world), 1, "not exactly one ClientHello");
    let identity = j
        .file_identity()
        .expect("the file holds the joined identity");
    let m = j.world.mediator();
    assert_eq!(
        m.hello(opened[0]),
        Some(HelloSeen {
            device: identity.d2m_device_id,
            expected: d2m::DeviceSlotState::New,
            exhausted: d2m::client_hello::DeviceSlotsExhaustedPolicy::Reject,
            expiration: d2m::DeviceSlotExpirationPolicy::Persistent,
        }),
        "the ClientHello is not the joined device's with expected NEW"
    );
    let sealed = m
        .device_info(&j.keys.path_public, identity.d2m_device_id)
        .expect("the slot keeps the device info");
    let plain = open_nonce_ahead(&j.keys.device_info, &sealed).expect("it opens with DGDIK");
    let info = d2d::DeviceInfo::decode(plain.as_slice()).expect("a DeviceInfo");
    assert!(
        info.label == j.label
            && j.label == "Kourier (j5a-host)"
            && identity.device_label == j.label,
        "the device info's label is not Kourier's"
    );
    assert_eq!(
        identity.slot_state,
        SlotState::Existing,
        "the slot is not existing in the file"
    );
    assert_eq!(j.file_rows("contact"), 3, "not 3 contacts in the file");
    assert_eq!(j.file_rows("grp"), 2, "not 2 groups in the file");
    assert_eq!(j.file_rows("core_outbox"), 0, "an acked row stayed");
    assert!(j.net().act(|m| m.release_dry(opened[0])), "no Dry was held");
    j.wait_state(
        "Online follower",
        |s| matches!(s, ConnState::Online { detail: Some(d) } if d == "follower"),
    )
    .await;
    j.no_more_steps().await;
    j.finish().await;
}

#[test]
fn registered_waits_for_the_core_to_apply_the_first_row() {
    support::offline::run_child(
        module_path!(),
        "registered_waits_for_the_core_to_apply_the_first_row_child",
    );
}

/// S3: the core side HOLDS the ack of zone 2's envelope (the one that carries row 1, `AccountReady`): the
/// phone hears NOTHING while it is held (no `Registered`, the rendezvous stays open), nor does the core
/// side get a login step; when the ack comes, `Registered` reaches the phone, and `Done` follows.
#[tokio::test]
async fn registered_waits_for_the_core_to_apply_the_first_row_child() {
    if !support::offline::in_child() {
        return;
    }
    let mut j = harness::start(setup(502));
    j.nominated().await;
    j.expect_step("linking", 3).await;
    let (first, ack) = j.durable().await;
    account_ready_first(&first);
    // Zone 2 is written and shipped; the core has not applied it. The fake hands on whatever Kourier
    // sent, so a `Registered` sent too early would be at the phone now.
    j.net().flush();
    j.sync().await;
    j.net().flush();
    assert!(
        j.joiner.ulp_received().is_empty(),
        "Registered went to the phone before the core applied AccountReady"
    );
    assert_eq!(
        j.joiner.closed(),
        None,
        "the phone saw the rendezvous close"
    );
    assert_eq!(j.rendezvous_closed(), None, "the rendezvous was closed");
    j.no_more_steps().await;
    ack.complete(Ok(()));
    j.wait_joiner(JoinStep::Closed).await;
    j.the_phone_got_registered_once_check();
    assert_eq!(j.rendezvous_closed(), Some(1000), "not closed 1000");
    // The batch was the whole of zone 2, so its ack also settled the last row: `Done` follows at once.
    j.expect_done(4).await;
    j.finish().await;
}

#[test]
fn done_waits_for_the_core_sides_ack() {
    support::offline::run_child(module_path!(), "done_waits_for_the_core_sides_ack_child");
}

/// C3 + S3: the rows come one by one (the core refused the first batch for good, so the forwarder isolates
/// them): once row 1 is APPLIED the phone gets `Registered`, but `Done` waits for the LAST row; it comes
/// right after that ack and carries the next txn.
#[tokio::test]
async fn done_waits_for_the_core_sides_ack_child() {
    if !support::offline::in_child() {
        return;
    }
    let mut j = harness::start(setup(508));
    j.nominated().await;
    j.expect_step("linking", 3).await;
    let (one, ack_one) = j.row_one().await;
    account_ready_first(&one);
    // Row 1 is held: nothing for the phone.
    j.net().flush();
    j.sync().await;
    j.net().flush();
    assert!(
        j.joiner.ulp_received().is_empty(),
        "Registered went to the phone before row 1 was applied"
    );
    ack_one.complete(Ok(()));
    j.wait_joiner(JoinStep::Closed).await;
    j.the_phone_got_registered_once_check();
    // Row 2 (the contacts and groups) is held: no `Done`.
    let (two, ack_two) = j.durable().await;
    assert!(
        two.iter()
            .all(|e| !matches!(e, BackendEvent::AccountReady { .. })),
        "row 2 is not the contacts and groups"
    );
    j.no_more_steps().await;
    ack_two.complete(Ok(()));
    j.expect_done(4).await;
    let mut got = one;
    got.extend(two);
    assert!(
        got == j.file_states(),
        "the core side got other events than zone 2 wrote"
    );
    j.finish().await;
}

#[test]
fn a_refused_first_row_ends_the_link_without_registered_or_done() {
    support::offline::run_child(
        module_path!(),
        "a_refused_first_row_ends_the_link_without_registered_or_done_child",
    );
}

/// P6: the core REFUSES row 1 (`AccountReady`) for good, here for a reason that is NOT a login refusal (a
/// contract error, `detail: "json"`: the core quarantines such a row and does not end the login itself): the
/// row is quarantined, and the forwarder's `acked` still moves past it. The actor tells the refusal from an
/// apply: NO `Registered`, the rendezvous closed 4100, the join's connection stopped, NO `Done`, and (TH5 L1,
/// decision C8) ONE `Failed { Internal }` at `linking`, so the login has an end whatever the core's reason.
/// The rest of zone 2 is shipped on (rows do not block each other); the step and row 2 race on the channel.
#[tokio::test]
async fn a_refused_first_row_ends_the_link_without_registered_or_done_child() {
    if !support::offline::in_child() {
        return;
    }
    let mut j = harness::start(setup(509));
    j.nominated().await;
    j.expect_step("linking", 3).await;
    let (_, ack_one) = j.row_one().await;
    let conn = mediator_opened(&j).await;
    J::refuse_as(ack_one, "json");
    // Row 2 still ships, alone, and is applied; the `Failed` step comes before or after it.
    let mut failed = Vec::new();
    for _ in 0..2 {
        let envelope = j.envelope().await;
        match envelope.ack {
            Some(ack) => ack.complete(Ok(())),
            None => failed.extend(envelope.events),
        }
    }
    match <[BackendEvent; 1]>::try_from(failed) {
        Ok([BackendEvent::Login(step)]) => {
            assert_eq!(step.step_id, "linking", "the failure is not at linking");
            assert_eq!(step.txn, 4, "the failure's txn does not rise by one");
            assert!(
                matches!(&step.kind, kourier_model::LoginStepKind::Failed { error }
                    if error.kind == ErrorKind::Internal),
                "the refused row 1 did not end the login with Failed Internal"
            );
        }
        _ => panic!("the refusal did not end the login with exactly one step"),
    }
    let net = j.net();
    assert!(
        net.wait(|| j.rendezvous_closed() == Some(4100), WALL).await,
        "the rendezvous was not closed 4100 after the refusal"
    );
    assert!(
        net.wait(|| j.world.mediator().closed(conn) == Some(1000), WALL)
            .await,
        "the join's connection was not closed 1000 after the refusal"
    );
    j.sync().await;
    j.no_more_steps().await;
    assert!(
        j.joiner.ulp_received().is_empty(),
        "the phone got Registered for a refused account"
    );
    assert_eq!(j.joiner.closed(), Some(4100), "the phone did not see 4100");
    assert_eq!(
        j.file_rows("core_outbox_poison"),
        1,
        "row 1 is not quarantined"
    );
    // Row 2's ack was completed above, but the forwarder deletes that row in a task of its own, and
    // nothing before this line waits for it (the 4100 and the close follow ROW 1's refusal, a `sync`
    // only the actor's command loop): wait for the delete, then the table is empty or the test fails.
    j.wait_rows("core_outbox", 0).await;
    j.finish().await;
}

#[test]
fn a_phone_that_left_first_still_gets_a_done_and_one_warn() {
    support::offline::run_child(
        module_path!(),
        "a_phone_that_left_first_still_gets_a_done_and_one_warn_child",
    );
}

/// P5: the phone leaves (closes the path) while the core's ack of row 1 is held. The device IS registered
/// on the mediator and the core owns the account, so the login still ends `Done` after the ack: no
/// `Registered` reached the phone, the actor logs ONE WARN (no content), the link is not failed.
#[tokio::test]
async fn a_phone_that_left_first_still_gets_a_done_and_one_warn_child() {
    if !support::offline::in_child() {
        return;
    }
    let warns = Warns::install();
    let mut j = harness::start(setup(511));
    j.nominated().await;
    j.expect_step("linking", 3).await;
    let (first, ack) = j.durable().await;
    account_ready_first(&first);
    // The phone goes; the fake hands the close on, so Kourier cannot write to the path any more.
    j.joiner.close(1000);
    j.net().flush();
    j.sync().await;
    ack.complete(Ok(()));
    j.expect_done(4).await;
    assert!(
        j.joiner.ulp_received().is_empty(),
        "a Registered reached a phone that had left"
    );
    let lines = warns.at(Level::WARN);
    let registered: Vec<&String> = lines.iter().filter(|m| m.contains("Registered")).collect();
    assert_eq!(
        registered.len(),
        1,
        "not exactly one WARN about the Registered that was not sent"
    );
    assert!(
        warns.at(Level::ERROR).is_empty(),
        "an ERROR line in a join the phone left"
    );
    j.finish().await;
}

#[test]
fn zone_two_waits_for_the_mediators_server_info() {
    support::offline::run_child(
        module_path!(),
        "zone_two_waits_for_the_mediators_server_info_child",
    );
}

/// H1: while the mediator withholds its ServerHello (so no `ServerInfo`), the file holds zone 1 only
/// (identity `new`, 3 contacts, 2 groups, NO core_outbox row), the core side got nothing and the phone
/// no `Registered`; the release lets the join end with `Done`.
#[tokio::test]
async fn zone_two_waits_for_the_mediators_server_info_child() {
    if !support::offline::in_child() {
        return;
    }
    let mut j = harness::start(setup(503));
    j.world.mediator().knobs(|k| k.withhold_server_hello = true);
    j.nominated().await;
    j.expect_step("linking", 3).await;
    let conn = mediator_opened(&j).await;
    j.sync().await;
    let identity = j.file_identity().expect("zone 1 wrote the identity");
    assert_eq!(
        identity.slot_state,
        SlotState::New,
        "the slot is not new before ServerInfo"
    );
    assert_eq!(j.file_rows("contact"), 3, "not 3 contacts after zone 1");
    assert_eq!(j.file_rows("grp"), 2, "not 2 groups after zone 1");
    assert_eq!(
        j.file_rows("core_outbox"),
        0,
        "zone 2 was written before ServerInfo"
    );
    assert!(
        j.joiner.ulp_received().is_empty(),
        "Registered went to the phone before ServerInfo"
    );
    j.no_more_steps().await;
    assert!(
        j.net().act(|m| m.release_server_hello(conn)),
        "no ServerHello was held"
    );
    let (first, ack) = j.durable().await;
    account_ready_first(&first);
    ack.complete(Ok(()));
    j.ack_until_done(first, 4).await;
    j.the_phone_got_registered_once_check();
    j.finish().await;
}

#[test]
fn a_taken_slot_fails_the_join_with_one_client_hello() {
    support::offline::run_child(
        module_path!(),
        "a_taken_slot_fails_the_join_with_one_client_hello_child",
    );
}

/// M1 on the mediator's REAL path: the drawn device's slot exists, so the mediator closes 4115 before
/// `ServerInfo`; L13's retry is OFF in a join: exactly ONE ClientHello, `Failed { Unauthorized }` at
/// `linking`, the rendezvous closed 4100, no `Registered`, nothing at the core side, no core_outbox row.
#[tokio::test]
async fn a_taken_slot_fails_the_join_with_one_client_hello_child() {
    if !support::offline::in_child() {
        return;
    }
    let mut j = harness::start(setup(504));
    j.world
        .mediator()
        .knobs(|k| k.slot_exists_at_next_hello = true);
    j.nominated().await;
    j.expect_step("linking", 3).await;
    j.expect_failed("linking", 4, &ErrorKind::Unauthorized)
        .await;
    assert_eq!(client_hellos(&j.world), 1, "not exactly one ClientHello");
    let opened = j.net().opened();
    assert_eq!(opened.len(), 1, "a second mediator connection");
    assert_eq!(
        j.world.mediator().closed(opened[0]),
        Some(4115),
        "the mediator did not close 4115"
    );
    assert_eq!(
        j.rendezvous_closed(),
        Some(4100),
        "the rendezvous was not closed 4100"
    );
    assert!(
        j.joiner.ulp_received().is_empty(),
        "the phone got Registered"
    );
    j.no_more_steps().await;
    assert_eq!(
        j.file_rows("core_outbox"),
        0,
        "a core_outbox row was written"
    );
    j.finish().await;
}

#[test]
fn no_server_info_in_twenty_seconds_fails_the_join() {
    support::offline::run_child(
        module_path!(),
        "no_server_info_in_twenty_seconds_fails_the_join_child",
    );
}

/// No `ServerInfo` within TH3's 20 s (the ServerHello withheld): `Failed { Transient }` at `linking`,
/// the mediator socket closed 1000 by Kourier, the rendezvous 4100, nothing at the core side.
#[tokio::test]
async fn no_server_info_in_twenty_seconds_fails_the_join_child() {
    if !support::offline::in_child() {
        return;
    }
    let mut j = harness::start(setup(505));
    j.world.mediator().knobs(|k| k.withhold_server_hello = true);
    j.nominated().await;
    j.expect_step("linking", 3).await;
    let conn = mediator_opened(&j).await;
    j.advance_to(START_MS + HANDSHAKE_WITHIN).await;
    j.expect_failed("linking", 4, &ErrorKind::Transient { retry_after: None })
        .await;
    assert_eq!(
        j.world.mediator().closed(conn),
        Some(1000),
        "the mediator socket was not closed 1000"
    );
    assert_eq!(j.net().attempts(), 2, "a second mediator connect");
    assert_eq!(
        j.rendezvous_closed(),
        Some(4100),
        "the rendezvous was not closed 4100"
    );
    assert!(
        j.joiner.ulp_received().is_empty(),
        "the phone got Registered"
    );
    j.no_more_steps().await;
    assert_eq!(
        j.file_rows("core_outbox"),
        0,
        "a core_outbox row was written"
    );
    j.finish().await;
}

#[test]
fn a_4110_on_the_join_socket_is_unsupported() {
    support::offline::run_child(
        module_path!(),
        "a_4110_on_the_join_socket_is_unsupported_child",
    );
}

/// P7: the mediator closes the join's first socket with 4110 (it does not take Kourier's protocol
/// version) before any `ServerInfo`: `Failed { Unsupported }` at `linking` (TH3 holds `UpdateApp` for
/// it; a join has no hold), not the `Unauthorized` of the other 41xx codes (4115 above); the
/// rendezvous closed 4100, no `Registered`, nothing at the core side.
#[tokio::test]
async fn a_4110_on_the_join_socket_is_unsupported_child() {
    if !support::offline::in_child() {
        return;
    }
    let mut j = harness::start(setup(507));
    j.world.mediator().knobs(|k| k.withhold_server_hello = true);
    j.nominated().await;
    j.expect_step("linking", 3).await;
    let conn = mediator_opened(&j).await;
    j.world.mediator().close(conn, 4110);
    j.net().flush();
    j.expect_failed("linking", 4, &ErrorKind::Unsupported).await;
    assert_eq!(client_hellos(&j.world), 0, "a ClientHello was sent");
    assert_eq!(
        j.rendezvous_closed(),
        Some(4100),
        "the rendezvous was not closed 4100"
    );
    assert!(
        j.joiner.ulp_received().is_empty(),
        "the phone got Registered"
    );
    j.no_more_steps().await;
    assert_eq!(
        j.file_rows("core_outbox"),
        0,
        "a core_outbox row was written"
    );
    j.finish().await;
}

#[test]
fn a_skewed_server_info_still_registers_then_holds_for_the_clock() {
    support::offline::run_child(
        module_path!(),
        "a_skewed_server_info_still_registers_then_holds_for_the_clock_child",
    );
}

/// L8: the fake's `ServerInfo` time is 25 min ahead: the device IS registered, so the join still
/// writes zone 2, sends `Registered` and is `Done` after the ack; the account then holds for the clock
/// (`NeedsUserAction(FixClock)`), as TH3 does.
#[tokio::test]
async fn a_skewed_server_info_still_registers_then_holds_for_the_clock_child() {
    if !support::offline::in_child() {
        return;
    }
    let mut j = harness::start(setup(506));
    j.world.mediator().knobs(|k| k.clock_skew_ms = SKEW_MS);
    j.nominated().await;
    j.expect_step("linking", 3).await;
    let (first, ack) = j.durable().await;
    account_ready_first(&first);
    ack.complete(Ok(()));
    let got = j.ack_until_done(first, 4).await;
    assert!(
        got == j.file_states(),
        "the core side got other events than zone 2 wrote"
    );
    j.the_phone_got_registered_once_check();
    assert_eq!(
        j.rendezvous_closed(),
        Some(1000),
        "the rendezvous was not closed 1000"
    );
    j.wait_state("FixClock", |s| {
        matches!(
            s,
            ConnState::NeedsUserAction(UserAction::FixClock { skew_ms }) if *skew_ms == -SKEW_MS
        )
    })
    .await;
    let identity = j
        .file_identity()
        .expect("the file holds the joined identity");
    assert_eq!(
        identity.slot_state,
        SlotState::Existing,
        "the slot is not existing in the file"
    );
    j.finish().await;
}

#[test]
fn a_spoiled_file_fails_zone_two_and_stops_the_connection() {
    support::offline::run_child(
        module_path!(),
        "a_spoiled_file_fails_zone_two_and_stops_the_connection_child",
    );
}

/// P4 + T1 + P1: the file is spoiled BETWEEN the zones (a second connection makes the contacts unreadable
/// while the mediator withholds its ServerHello), then the ServerHello is released. Zone 2 fails:
/// `Failed { Internal }` at `linking`, NO `Registered` (nothing reached the phone), the rendezvous closed
/// 4100, NO `core_outbox` row, no marker, and the join's mediator connection STOPPED (closed 1000 by
/// Kourier). No production failpoint: the failure is real data the file cannot read.
#[tokio::test]
async fn a_spoiled_file_fails_zone_two_and_stops_the_connection_child() {
    if !support::offline::in_child() {
        return;
    }
    let mut j = harness::start(setup(512));
    j.world.mediator().knobs(|k| k.withhold_server_hello = true);
    j.nominated().await;
    j.expect_step("linking", 3).await;
    let conn = mediator_opened(&j).await;
    j.sync().await;
    j.spoil_contacts();
    assert!(
        j.net().act(|m| m.release_server_hello(conn)),
        "no ServerHello was held"
    );
    j.expect_failed("linking", 4, &ErrorKind::Internal).await;
    assert!(
        j.joiner.ulp_received().is_empty(),
        "Registered went to the phone although zone 2 failed"
    );
    assert_eq!(
        j.rendezvous_closed(),
        Some(4100),
        "the rendezvous was not closed 4100"
    );
    assert_eq!(
        j.world.mediator().closed(conn),
        Some(1000),
        "the join's connection stayed open after the failed zone 2"
    );
    assert_eq!(
        j.file_rows("core_outbox"),
        0,
        "a core_outbox row was written"
    );
    assert_eq!(j.file_rows("join_ready"), 0, "the marker was written");
    j.no_more_steps().await;
    j.finish().await;
}

#[test]
fn a_server_info_that_is_not_new_fails_the_join() {
    support::offline::run_child(
        module_path!(),
        "a_server_info_that_is_not_new_fails_the_join_child",
    );
}

/// P2 (the belt of decision M1): the mediator answers the `ClientHello` expecting NEW with a `ServerInfo`
/// that says EXISTING (SPEC-IMPOSSIBLE: the fake's knob `server_info_slot_state`, belt only). The join
/// fails `Unauthorized` at `linking`, the mediator socket is closed 1000 by Kourier, the rendezvous
/// 4100, no zone 2 (no row, no marker), no `Registered`, no more ClientHello.
#[tokio::test]
async fn a_server_info_that_is_not_new_fails_the_join_child() {
    if !support::offline::in_child() {
        return;
    }
    let mut j = harness::start(setup(513));
    j.world
        .mediator()
        .knobs(|k| k.server_info_slot_state = Some(d2m::DeviceSlotState::Existing));
    j.nominated().await;
    j.expect_step("linking", 3).await;
    j.expect_failed("linking", 4, &ErrorKind::Unauthorized)
        .await;
    let opened = j.net().opened();
    assert_eq!(opened.len(), 1, "not one mediator connection");
    assert_eq!(client_hellos(&j.world), 1, "not exactly one ClientHello");
    assert_eq!(
        j.world.mediator().closed(opened[0]),
        Some(1000),
        "the mediator socket was not closed 1000 by Kourier"
    );
    assert_eq!(
        j.rendezvous_closed(),
        Some(4100),
        "the rendezvous was not closed 4100"
    );
    assert!(
        j.joiner.ulp_received().is_empty(),
        "the phone got Registered"
    );
    assert_eq!(j.file_rows("core_outbox"), 0, "zone 2 wrote a row");
    assert_eq!(j.file_rows("join_ready"), 0, "zone 2 wrote the marker");
    j.no_more_steps().await;
    j.finish().await;
}

#[test]
fn a_second_start_login_after_registered_is_ignored() {
    support::offline::run_child(
        module_path!(),
        "a_second_start_login_after_registered_is_ignored_child",
    );
}

/// P9: a second `StartLogin` while the join is past zone 2 (here: `Registered` is sent, the last row's ack
/// is held) changes nothing: no step (the counter is not reset, nothing fails at `qr`), no second relay
/// socket; the `Done` that follows carries the txn the join had reached.
#[tokio::test]
async fn a_second_start_login_after_registered_is_ignored_child() {
    if !support::offline::in_child() {
        return;
    }
    let mut j = harness::start(setup(514));
    j.nominated().await;
    j.expect_step("linking", 3).await;
    let (one, ack_one) = j.row_one().await;
    account_ready_first(&one);
    ack_one.complete(Ok(()));
    j.wait_joiner(JoinStep::Closed).await;
    let (_, ack_two) = j.durable().await;
    j.send(harness::start_login()).await;
    j.sync().await;
    j.no_more_steps().await;
    assert_eq!(
        j.net().relay_opened().len(),
        1,
        "a second StartLogin opened a second rendezvous socket"
    );
    ack_two.complete(Ok(()));
    j.expect_done(4).await;
    j.finish().await;
}

#[test]
fn connect_after_a_failed_join_does_nothing() {
    support::offline::run_child(
        module_path!(),
        "connect_after_a_failed_join_does_nothing_child",
    );
}

/// S11: the join failed past zone 1 (the mediator closed 4115 before `ServerInfo`): the file keeps the
/// identity, but a `Connect` (the core sends one on a UI change, linked or not) makes NO mediator request:
/// registering the device on the mediator is what the failure was to avoid.
#[tokio::test]
async fn connect_after_a_failed_join_does_nothing_child() {
    if !support::offline::in_child() {
        return;
    }
    let mut j = harness::start(setup(515));
    j.world
        .mediator()
        .knobs(|k| k.slot_exists_at_next_hello = true);
    j.nominated().await;
    j.expect_step("linking", 3).await;
    j.expect_failed("linking", 4, &ErrorKind::Unauthorized)
        .await;
    assert!(j.identity_exists(), "zone 1's identity is gone");
    let before = j.net().attempts();
    j.send(Command::Connect).await;
    for _ in 0..5 {
        j.sync().await;
    }
    assert_eq!(
        j.net().attempts(),
        before,
        "a Connect after the failed join asked for a connection"
    );
    assert_eq!(j.net().opened().len(), 1, "a second mediator connection");
    j.no_more_steps().await;
    j.finish().await;
}

#[test]
fn connect_after_a_cancelled_join_does_nothing() {
    support::offline::run_child(
        module_path!(),
        "connect_after_a_cancelled_join_does_nothing_child",
    );
}

/// S11: the person cancels past zone 1 (the ServerHello withheld: the join's connection is still waiting
/// for the mediator); a `Connect` afterwards makes no mediator request.
#[tokio::test]
async fn connect_after_a_cancelled_join_does_nothing_child() {
    if !support::offline::in_child() {
        return;
    }
    let mut j = harness::start(setup(516));
    j.world.mediator().knobs(|k| k.withhold_server_hello = true);
    j.nominated().await;
    j.expect_step("linking", 3).await;
    let conn = mediator_opened(&j).await;
    j.send(Command::CancelLogin).await;
    j.sync().await;
    assert_eq!(
        j.world.mediator().closed(conn),
        Some(1000),
        "the cancel did not close the join's connection"
    );
    let before = j.net().attempts();
    j.send(Command::Connect).await;
    for _ in 0..5 {
        j.sync().await;
    }
    assert_eq!(
        j.net().attempts(),
        before,
        "a Connect after the cancelled join asked for a connection"
    );
    assert_eq!(j.net().opened().len(), 1, "a second mediator connection");
    j.no_more_steps().await;
    // RP-b: the core's `Logout` drops the device from zone 1's identity on sockets of its own that expect EXISTING. The
    // join's ClientHello never went (the ServerHello was withheld), so the mediator has no slot: 4115. The file still
    // says `new` (a hello of the join may have been in flight, P-M2), so the first 4115 is not final: ONE more socket one
    // second of the actor's clock later is told 4115 too: `Removed` (the network does not know the device), and no slot
    // is made on the way.
    j.world
        .mediator()
        .knobs(|k| k.withhold_server_hello = false);
    let label = j.label.clone();
    let at = j.now() + 1_000;
    j.send(Command::Logout).await;
    j.advance_to(at).await;
    j.reported(DeviceLogout::Removed, Some(&label)).await;
    let opened = j.net().opened();
    assert_eq!(opened.len(), 3, "not two sockets of the drop's own");
    let m = j.world.mediator();
    for socket in &opened[1..] {
        assert_eq!(
            m.hello(*socket).map(|hello| hello.expected),
            Some(d2m::DeviceSlotState::Existing),
            "a drop's socket did not expect EXISTING"
        );
        assert_eq!(
            m.closed(*socket),
            Some(4115),
            "a drop's socket was not told 4115"
        );
    }
    assert!(
        m.slots(&j.keys.path_public).is_empty(),
        "the logout made a slot"
    );
    assert!(m.drops().is_empty(), "a drop went without a slot");
    j.world.violations().assert_empty();
}

impl J {
    /// [`the_phone_got_registered_once`] as a method (reads better in the tests).
    fn the_phone_got_registered_once_check(&self) {
        the_phone_got_registered_once(self);
    }
}
