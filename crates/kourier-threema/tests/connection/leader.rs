//! TH6 L4b: THE LEADER STAGE against the offline fake. The phone is offline and the mediator promotes Kourier: the
//! socket unblocks ONCE (after Dry was counted), takes every message the chat server delivers, writes it in ZONE
//! RECEIVE, reflects it, and acks it to the chat server only after its reflect-ack made it `reflected` (the M5 order;
//! THE GUARD ORACLE of the harness's `finish` checks every ack against the fake's record, and the first test holds the
//! reflect-acks back so that the order is PROVED: an ack before its reflect-ack would show while they are held). A
//! typing indicator is reflected ephemerally and acked only when its flags lack `0x04` (TH6 FIX-A). A follower never
//! unblocks; a promotion in mid-socket
//! unblocks once. The CSP backlog is a catch-up (critic T-M2): `Syncing` until the chat server's queue was sent and the
//! leader is idle, or 10 quiet seconds without that word. A backlog over the FIFO's bound is HELD and the socket is
//! recycled once it drained, never a loop (critic T-H2), and once a box was refused nothing later is taken on that
//! socket (TH6 FIX-A, the prefix rule). A reflect that stays unanswered through two sockets cools
//! (critic T-M4). Every count line equals the fake's trace; no line names an ID.

use std::time::Duration;

use kourier_backend_api::{BackendEvent, Command};
use kourier_model::ConnState;

use super::harness::{self, DEVICE, ME, SERVER_GROUP, Scene, scene, traced};
use super::lines::{
    COUNT, WALL, at, count_line, counters, counts_of, equals_the_trace, no_id, online_as, retry_at,
    wait_lines,
};
use super::reflect::PHONE;
use super::send::through_echoes;
use crate::support;
use crate::support::mock::body::{self, Place, ty};
use crate::support::mock::csp::flag;
use crate::support::mock::d2m::EPHEMERAL;
use crate::support::mock::peer::Letter;
use crate::support::warns::{Line, Warns};

/// The peers (synthetic IDs, each a stored contact with a real key).
const ANN: &str = "PEER0001";
const BEN: &str = "PEER0002";
const CAT: &str = "PEER0003";
/// ANN's group, BEN a member.
const GID: u64 = 0x0102_0304_0506_0708;
/// CAT's group, which the file does not hold.
const CATS: u64 = 0x0303_0303_0303_0303;
/// D2's words.
const HELD: &str = "a chat server message is held for the phone";
/// The upper bound of the backlog's wait (300 messages through the real steps and zones in a debug build).
const BACKLOG_WALL: Duration = Duration::from_secs(60);

fn syncing(s: &ConnState) -> bool {
    matches!(s, ConnState::Syncing { .. })
}

/// The scene of the first test: ANN a contact, ANN's group with BEN.
fn with_group(seed: u64) -> Scene {
    scene(seed)
        .with_peer(ANN)
        .with_members(ANN, GID, &[ANN, BEN])
}

/// Waits (a wall BOUND) until the fake took `n` reflects in all.
async fn wait_reflects(h: &harness::H, n: usize) {
    let m = h.s.world.mediator().clone();
    assert!(
        h.net().wait(|| m.reflects().len() >= n, WALL).await,
        "the fake took {} reflects, not {n}",
        m.reflects().len()
    );
    assert_eq!(m.reflects().len(), n, "more reflects than expected");
}

/// Waits (a wall BOUND) until `conn` sent `n` CSP acks of incoming messages.
async fn wait_acks(h: &harness::H, conn: support::mock::d2m::ConnId, n: usize) {
    let world = &h.s.world;
    assert!(
        h.net()
            .wait(
                || traced(world, conn, "in", "csp-incoming-message-ack") >= n,
                WALL
            )
            .await,
        "{} of {n} acks came",
        traced(world, conn, "in", "csp-incoming-message-ack")
    );
    assert_eq!(
        traced(world, conn, "in", "csp-incoming-message-ack"),
        n,
        "more acks than expected"
    );
}

#[test]
fn the_leader_reflects_a_text_a_group_text_a_location_a_receipt_and_typing_then_acks_each() {
    support::offline::run_child(
        module_path!(),
        "the_leader_reflects_a_text_a_group_text_a_location_a_receipt_and_typing_then_acks_each_child",
    );
}

/// THE M5 ORDER (MUTANT M1, the CSP ack before the reflect-ack, dies here): the phone offline, five messages wait at the
/// chat server (a text, a group text, a location and a delivery receipt, then a typing indicator that is never acked).
/// Kourier is promoted at its login, unblocks once and reflects all five (the typing EPHEMERALLY); while the mediator
/// holds the reflect-acks, NOTHING is acked and the account stays `Syncing` (T-M2: the CSP backlog is a catch-up). Once
/// the reflect-acks come, the four are acked; the core gets the three messages and the receipt, and once it acked them,
/// `Online` as leader; the delivery receipt for the text and the location goes to ANN as a job (one job for both when
/// the reflect-acks came in one pass, one per id when in two: the union of the ids is what is asserted). The phone
/// back as follower views the four reflections in order. The socket's end line equals the fake's trace.
#[tokio::test]
async fn the_leader_reflects_a_text_a_group_text_a_location_a_receipt_and_typing_then_acks_each_child()
 {
    if !support::offline::in_child() {
        return;
    }
    let s = with_group(9_501);
    let m = s.world.mediator().clone();
    m.knobs(|k| k.withhold_reflect_acks = true);
    let mut ann = s.world.peer(ANN);
    let mut ben = s.world.peer(BEN);
    ann.send(&Letter::text(ME, 0x11, "synthetic text"));
    ben.send(&Letter::new(
        ME,
        0x12,
        ty::GROUP_TEXT,
        &body::group_member_container(ANN, GID, &body::text("synthetic group text")),
    ));
    ann.send(&Letter::new(
        ME,
        0x13,
        ty::LOCATION,
        &body::location("47.3769", "8.5417", None, &Place::Nothing),
    ));
    ann.send(&Letter::new(
        ME,
        0x14,
        ty::DELIVERY_RECEIPT,
        &body::delivery_receipt(1, &[[7; 8]]),
    ));
    ann.send(
        &Letter::new(ME, 0x15, ty::TYPING_INDICATOR, &body::typing(1)).flags(flag::NO_SERVER_ACK),
    );
    // The phone has its slot and is offline: the mediator keeps Kourier's reflections for it.
    let mut phone = s.world.phone(&s.dgk, SERVER_GROUP, PHONE);
    phone.connect();
    phone.disconnect();
    let warns = Warns::install_info();
    let mut h = harness::start(s);
    h.send(Command::Connect).await;
    let conn = h.wait_connected().await;
    wait_reflects(&h, 5).await;
    h.sync().await;
    assert_eq!(
        traced(&h.s.world, conn, "in", "csp-incoming-message-ack"),
        0,
        "an ack before its reflect-ack"
    );
    assert_eq!(
        m.reflect_acks_held(conn),
        4,
        "the four reflects owed an ack"
    );
    assert!(syncing(&h.state()), "not Syncing: {:?}", h.state());
    assert_eq!(
        traced(&h.s.world, conn, "in", "csp-unblock-incoming-messages"),
        1,
        "not one unblock"
    );
    m.knobs(|k| k.withhold_reflect_acks = false);
    assert_eq!(h.net().act(|m| m.release_reflect_acks(conn)), 4);
    wait_acks(&h, conn, 4).await;
    let events = h.ack_until("Online as leader", online_as("leader")).await;
    let messages = events
        .iter()
        .filter(|e| matches!(e, BackendEvent::Message(_)))
        .count();
    let receipts = events
        .iter()
        .filter(|e| matches!(e, BackendEvent::Receipts { .. }))
        .count();
    assert_eq!((messages, receipts), (3, 1), "what the core got");
    // The jobs of the coalesced receipts run at `Online`: ZONE REFLECTED coalesces one receipt per sender PER ZONE, and
    // the fake writes the four reflect-acks one by one while the actor drains what has arrived, so they come in one
    // pass or in two: ANN gets ONE job (the text's and the location's ids) or TWO (one id each). Either way the ids
    // are the text's and the location's, each once and in order, and never a receipt for the delivery receipt (0x14).
    let mut jobs: Vec<Vec<u8>> = Vec::new();
    let ids_in = |jobs: &[Vec<u8>]| jobs.iter().map(|body| (body.len() - 1) / 8).sum::<usize>();
    assert!(
        h.net()
            .wait(
                || {
                    for one in ann.drain() {
                        assert_eq!(
                            one.ty,
                            ty::DELIVERY_RECEIPT,
                            "a box to ANN that is no receipt"
                        );
                        jobs.push(one.body);
                    }
                    ids_in(&jobs) >= 2
                },
                WALL
            )
            .await,
        "the receipts did not go: {} ids in {} jobs",
        ids_in(&jobs),
        jobs.len()
    );
    assert!(
        (1..=2).contains(&jobs.len()),
        "{} receipt jobs for two ids",
        jobs.len()
    );
    let mut ids = Vec::new();
    for body in &jobs {
        assert_eq!(body[0], 1, "the receipt's status");
        let (chunks, rest) = body[1..].as_chunks::<8>();
        assert!(rest.is_empty(), "a receipt body of whole ids");
        ids.extend(chunks.iter().map(|id| u64::from_le_bytes(*id)));
    }
    assert_eq!(ids, [0x11, 0x13], "the receipts' ids");
    phone.connect();
    phone.incoming();
    let viewed = phone.view().incoming_ids();
    let want: Vec<(String, u64)> = [(ANN, 0x11), (BEN, 0x12), (ANN, 0x13), (ANN, 0x14)]
        .iter()
        .map(|(id, n)| ((*id).to_owned(), *n))
        .collect();
    assert_eq!(viewed, want, "the phone's view of the reflections");
    assert!(h.net().close_device(DEVICE, 4000), "no socket to close");
    let end = wait_lines(&warns, 1, count_line("end")).await;
    let counted = counters(&end[0]);
    assert_eq!(
        (
            counted["out_unblock"],
            counted["in_csp_message"],
            counted["out_reflect_inbound"],
            counted["out_ephemeral"],
            counted["out_message_ack"],
            counted["in_queue_complete"],
        ),
        (1, 5, 4, 1, 4, 1),
        "the leader's frames"
    );
    equals_the_trace(&h, conn, &end[0]);
    no_id(&warns);
    h.finish().await;
}

#[test]
fn a_follower_never_unblocks_and_a_promotion_in_mid_socket_unblocks_once() {
    support::offline::run_child(
        module_path!(),
        "a_follower_never_unblocks_and_a_promotion_in_mid_socket_unblocks_once_child",
    );
}

/// A FOLLOWER never asks for messages (a minute with a message waiting at the chat server); promoted in mid-socket, it
/// goes `Syncing` (critic T-M2: the CSP backlog is a catch-up), unblocks ONCE, reflects and acks the message, and is
/// `Online` as leader once the core has it.
#[tokio::test]
async fn a_follower_never_unblocks_and_a_promotion_in_mid_socket_unblocks_once_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(9_502).with_peer(ANN);
    s.world.mediator().knobs(|k| k.auto_leader = false);
    let mut ann = s.world.peer(ANN);
    let mut h = harness::start(s);
    h.send(Command::Connect).await;
    h.wait_state("Online as follower", online_as("follower"))
        .await;
    let conn = h.conn();
    ann.send(&Letter::text(ME, 0x21, "synthetic text"));
    for n in 1..=6 {
        h.advance_to(at(10 * n)).await;
    }
    h.sync().await;
    let m = h.s.world.mediator().clone();
    assert_eq!(
        traced(&h.s.world, conn, "in", "csp-unblock-incoming-messages"),
        0,
        "a follower unblocked"
    );
    assert_eq!(m.csp_queue_len(ME), 1, "the message did not wait");
    h.net().act(|m| m.promote(conn));
    let events = h.ack_until("Online as leader", online_as("leader")).await;
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, BackendEvent::Message(_)))
            .count(),
        1,
        "the message did not reach the core"
    );
    assert_eq!(
        traced(&h.s.world, conn, "in", "csp-unblock-incoming-messages"),
        1,
        "not one unblock"
    );
    wait_acks(&h, conn, 1).await;
    assert_eq!(m.csp_queue_len(ME), 0, "the message is still queued");
    h.finish().await;
}

#[test]
fn without_queue_send_complete_the_catch_up_ends_after_ten_quiet_seconds() {
    support::offline::run_child(
        module_path!(),
        "without_queue_send_complete_the_catch_up_ends_after_ten_quiet_seconds_child",
    );
}

/// Critic T-M2's FALLBACK: the chat server never says its queue was sent. The leader unblocks, has nothing to do and
/// stays `Syncing` for 10 seconds of the actor's clock, then counts the boundary and is `Online` as leader.
#[tokio::test]
async fn without_queue_send_complete_the_catch_up_ends_after_ten_quiet_seconds_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(9_503);
    s.world
        .mediator()
        .knobs(|k| k.csp.withhold_queue_send_complete = true);
    let mut h = harness::start(s);
    h.send(Command::Connect).await;
    let conn = h.wait_connected().await;
    let world = &h.s.world;
    assert!(
        h.net()
            .wait(
                || traced(world, conn, "in", "csp-unblock-incoming-messages") == 1,
                WALL
            )
            .await,
        "no unblock"
    );
    h.wait_deadline(at(10)).await;
    h.sync().await;
    assert!(syncing(&h.state()), "not Syncing: {:?}", h.state());
    h.advance_to(at(10)).await;
    h.wait_state("Online as leader", online_as("leader")).await;
    assert_eq!(
        traced(&h.s.world, conn, "out", "csp-queue-send-complete"),
        0,
        "the chat server said it"
    );
    h.finish().await;
}

#[test]
fn a_backlog_of_300_is_held_over_the_fifo_and_taken_by_the_next_socket_without_a_loop() {
    support::offline::run_child(
        module_path!(),
        "a_backlog_of_300_is_held_over_the_fifo_and_taken_by_the_next_socket_without_a_loop_child",
    );
}

/// Critic T-H2 (ITS MUTANT, an overflow through `HeldFull`, dies here): 300 texts of three peers wait while the phone is
/// offline. The chat server sends all of them at the unblock, ahead of the first reflect-ack, so the FIFO (256) and the
/// batch in flight (30) hold 286: the other 14 are HELD (`fifo-full`), never a close. Once the 286 are reflected and
/// acked and the FIFO drained, the socket is recycled at once (not counted, no wait on the clock), and the next socket
/// takes the 14. Two sockets, every message acked, no time passed.
#[tokio::test]
async fn a_backlog_of_300_is_held_over_the_fifo_and_taken_by_the_next_socket_without_a_loop_child()
{
    if !support::offline::in_child() {
        return;
    }
    let s = scene(9_504).with_peer(ANN).with_peer(BEN).with_peer(CAT);
    let mut peers = [s.world.peer(ANN), s.world.peer(BEN), s.world.peer(CAT)];
    for n in 0..300_u64 {
        let peer = &mut peers[usize::try_from(n % 3).expect("fits")];
        peer.send(&Letter::text(ME, 0x1000 + n, "synthetic backlog"));
    }
    let m = s.world.mediator().clone();
    assert_eq!(m.csp_queue_len(ME), 300);
    let warns = Warns::install_info();
    let h = harness::start(s);
    h.send(Command::Connect).await;
    assert!(
        h.net()
            .wait(|| m.csp_queue_len(ME) == 0, BACKLOG_WALL)
            .await,
        "{} messages still queued",
        m.csp_queue_len(ME)
    );
    let opened = h.net().opened();
    assert_eq!(opened.len(), 2, "not two sockets");
    let (first, second) = (opened[0], opened[1]);
    let end = wait_lines(&warns, 1, count_line("end")).await;
    let counted = counts_of(&end[0]);
    assert_eq!(
        (
            counted["in_csp_message"],
            counted["out_reflect_inbound"],
            counted["out_message_ack"]
        ),
        (300, 286, 286),
        "the first socket's messages"
    );
    assert_eq!(
        (end[0].field("held"), end[0].field("held_by")),
        (Some("14"), Some("fifo-full:14")),
        "the held boxes"
    );
    equals_the_trace(&h, first, &end[0]);
    wait_acks(&h, second, 14).await;
    let held = warns
        .lines()
        .into_iter()
        .filter(|l| l.message == HELD && l.field("rule") == Some("fifo-full"))
        .count();
    assert_eq!(held, 14, "one D2 line per held box");
    assert_eq!(h.now(), at(0), "a wait on the clock");
    assert!(
        traced(&h.s.world, first, "in", "close") == 1,
        "the first socket was not the client's to close"
    );
    no_id(&warns);
    h.finish().await;
}

#[test]
fn a_reflect_unanswered_through_two_sockets_cools_and_is_held() {
    support::offline::run_child(
        module_path!(),
        "a_reflect_unanswered_through_two_sockets_cools_and_is_held_child",
    );
}

/// Critic T-M4: the mediator never acks the reflect of one text. The first socket closes at `ACK_WITHIN` (+60 s), the
/// second resumes the `pending` row with the SAME sealed bytes (never sealed again) and closes at +125 s; the third
/// holds it (`inbound-reflect-unanswered`): no reflect, no ack, the socket stays. Nothing is ever acked.
///
/// The clocks walk to each close through the echoes (`through_echoes`): every stop waits until the actor waits for it, so
/// each echo of the keep-alive (every 10 s) is written, and seen by the fake at its virtual time, before the time moves
/// on. A single `step_to` does not wait for the actor's thread: on a loaded machine the fake's virtual clock passed the
/// CHAT SERVER's idle timer (30 s after the last CSP bytes, `Mediator::tick`) before the actor had sent its echo, and the
/// fake closed the socket (4000) at +128 s instead of the ack deadline: the retry came at +133 s, not +130 s (L6, one
/// strike in three runs; the proof is in `th6-progress.md`, "L4b flakes").
#[tokio::test]
async fn a_reflect_unanswered_through_two_sockets_cools_and_is_held_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(9_505).with_peer(ANN);
    let m = s.world.mediator().clone();
    m.knobs(|k| k.withhold_reflect_acks = true);
    let mut ann = s.world.peer(ANN);
    ann.send(&Letter::text(ME, 0x51, "synthetic text"));
    let warns = Warns::install_info();
    let mut h = harness::start(s);
    h.send(Command::Connect).await;
    h.wait_connected().await;
    wait_reflects(&h, 1).await;
    through_echoes(&h, at(0), at(60)).await;
    h.wait_state("Offline with a retry", retry_at(at(65))).await;
    h.advance_to(at(65)).await;
    h.wait_opened(2).await;
    wait_reflects(&h, 2).await;
    through_echoes(&h, at(65), at(125)).await;
    h.wait_state("Offline with a retry", retry_at(at(130)))
        .await;
    h.advance_to(at(130)).await;
    h.wait_opened(3).await;
    let third = h.wait_connected().await;
    let cooled =
        |l: &Line| l.message == HELD && l.field("rule") == Some("inbound-reflect-unanswered");
    wait_lines(&warns, 1, cooled).await;
    let reflects = m.reflects();
    assert_eq!(reflects.len(), 2, "the third socket reflected");
    assert_eq!(
        reflects[0].bytes, reflects[1].bytes,
        "the resumed reflect was sealed again"
    );
    assert_eq!(
        traced(&h.s.world, third, "in", "csp-incoming-message-ack"),
        0,
        "the cooled message was acked"
    );
    assert_eq!(m.csp_queue_len(ME), 1, "the message left the server");
    assert!(h.net().close_device(DEVICE, 4000), "no socket to close");
    let ends = wait_lines(&warns, 3, count_line("end")).await;
    assert_eq!(
        (ends[2].field("held"), ends[2].field("held_by")),
        (Some("1"), Some("inbound-reflect-unanswered:1")),
        "the third socket's hold"
    );
    assert_eq!(counts_of(&ends[2])["out_reflect_inbound"], 0);
    equals_the_trace(&h, third, &ends[2]);
    assert!(ends.iter().all(|l| l.message == COUNT));
    no_id(&warns);
    h.finish().await;
}

/// Waits (`wall`) until exactly `n` captured lines match `what`.
async fn wait_long(warns: &Warns, n: usize, what: impl Fn(&Line) -> bool, wall: Duration) {
    let found = || warns.lines().into_iter().filter(|l| what(l)).count();
    let reached = tokio::time::timeout(wall, async {
        while found() < n {
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert!(reached.is_ok(), "{} of {n} lines came", found());
    assert_eq!(found(), n, "more lines than expected");
}

#[test]
fn a_box_refused_for_room_holds_every_later_box_and_nothing_overtakes_it() {
    support::offline::run_child(
        module_path!(),
        "a_box_refused_for_room_holds_every_later_box_and_nothing_overtakes_it_child",
    );
}

/// TH6 FIX-A, S-B1 (a), THE PREFIX RULE (ITS MUTANT, the rule off, dies here): 300 messages wait, the 290th CAT's
/// `group-setup` of a group the file does not hold, with the user and BEN; the mediator holds the reflect-acks. The batch
/// in flight (30) and the FIFO (256) take 286; the other 14, the setup among them, are HELD `fifo-full`. One batch's
/// reflect-acks come: its 30 are acked and the next batch drains room. Then BEN writes in CAT's group and ANN sends a
/// text, live: the socket refused a box before, so both are HELD `fifo-full` too, although the FIFO has room (without
/// the rule BEN's text could be discarded `group-unknown` and acked ahead of the setup that makes its group: lost for
/// every device, and ANN's text overtake it). The rest released, the first socket acks 286, holds 16 and is recycled
/// once drained; the second gets the 16 in the server's order: the setup is held for the phone (`group-sync-needed`),
/// BEN's text behind it (`group-held-before`), the 14 others reflected and acked.
#[tokio::test]
async fn a_box_refused_for_room_holds_every_later_box_and_nothing_overtakes_it_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(9_507).with_peer(ANN).with_peer(BEN).with_peer(CAT);
    let m = s.world.mediator().clone();
    m.knobs(|k| k.withhold_reflect_acks = true);
    let mut ann = s.world.peer(ANN);
    let mut ben = s.world.peer(BEN);
    let mut cat = s.world.peer(CAT);
    for n in 0..300_u64 {
        let id = 0x1000 + n;
        if n == 289 {
            cat.send(&Letter::new(
                ME,
                id,
                ty::GROUP_SETUP,
                &body::group_creator_container(CATS, &body::group_setup(&[ME, BEN])),
            ));
        } else if n % 2 == 0 {
            ann.send(&Letter::text(ME, id, "synthetic backlog"));
        } else {
            ben.send(&Letter::text(ME, id, "synthetic backlog"));
        }
    }
    assert_eq!(m.csp_queue_len(ME), 300);
    let warns = Warns::install_info();
    let h = harness::start(s);
    h.send(Command::Connect).await;
    let first = h.wait_connected().await;
    let fifo_full = |l: &Line| l.message == HELD && l.field("rule") == Some("fifo-full");
    wait_long(&warns, 14, fifo_full, BACKLOG_WALL).await;
    wait_reflects(&h, 30).await;
    h.sync().await;
    assert_eq!(
        traced(&h.s.world, first, "in", "csp-incoming-message-ack"),
        0,
        "an ack before its reflect-ack"
    );
    // One batch's reflect-acks: its 30 are acked, the next batch drains room (its reflect-acks held again).
    assert_eq!(h.net().act(|m| m.release_reflect_acks(first)), 30);
    wait_acks(&h, first, 30).await;
    wait_reflects(&h, 60).await;
    // Two letters come live while the FIFO has room.
    h.net().act(|_| {
        ben.send(&Letter::new(
            ME,
            0x2001,
            ty::GROUP_TEXT,
            &body::group_member_container(CAT, CATS, &body::text("synthetic group text")),
        ));
        ann.send(&Letter::text(ME, 0x2002, "synthetic live text"));
    });
    wait_lines(&warns, 16, fifo_full).await;
    m.knobs(|k| k.withhold_reflect_acks = false);
    assert_eq!(h.net().act(|m| m.release_reflect_acks(first)), 30);
    assert!(
        h.net()
            .wait(
                || h.net().opened().len() == 2 && m.csp_queue_len(ME) == 2,
                BACKLOG_WALL
            )
            .await,
        "{} messages queued, {} sockets",
        m.csp_queue_len(ME),
        h.net().opened().len()
    );
    let second = h.net().opened()[1];
    wait_acks(&h, second, 14).await;
    let held = |l: &Line| l.message == HELD && l.field("rule") != Some("fifo-full");
    let later = wait_lines(&warns, 2, held).await;
    let rules: Vec<&str> = later.iter().filter_map(|l| l.field("rule")).collect();
    assert_eq!(
        rules,
        ["group-sync-needed", "group-held-before"],
        "the second socket's holds"
    );
    assert!(h.net().close_device(DEVICE, 4000), "no socket to close");
    let ends = wait_lines(&warns, 2, count_line("end")).await;
    for (end, conn, want, held_by) in [
        (&ends[0], first, (302, 286, 286), "fifo-full:16"),
        (
            &ends[1],
            second,
            (16, 14, 14),
            "group-held-before:1,group-sync-needed:1",
        ),
    ] {
        let counted = counts_of(end);
        assert_eq!(
            (
                counted["in_csp_message"],
                counted["out_reflect_inbound"],
                counted["out_message_ack"]
            ),
            want,
            "the socket's messages"
        );
        let held: u64 = want.0 - want.2;
        assert_eq!(
            (end.field("held"), end.field("held_by")),
            (Some(held.to_string().as_str()), Some(held_by)),
            "the socket's holds"
        );
        equals_the_trace(&h, conn, end);
    }
    assert_eq!(h.now(), at(0), "a wait on the clock");
    no_id(&warns);
    h.finish().await;
}

#[test]
fn a_typing_indicator_that_owes_an_ack_is_acked_after_its_ephemeral_reflect() {
    support::offline::run_child(
        module_path!(),
        "a_typing_indicator_that_owes_an_ack_is_acked_after_its_ephemeral_reflect_child",
    );
}

/// TH6 FIX-A, S-L3 = P-L2: two typing indicators come live to the leader (the chat server queues none while nobody
/// takes messages: `0x02`). The one WITHOUT `0x04` is reflected ephemerally and then ACKED (the spec's step 25,
/// Desktop's acks by the flags), so the chat server drops it; the one with `0x04` is reflected and never acked (the
/// fake records an ack it does not expect). Nothing is held; the guard oracle in `finish` takes the ack after the
/// ephemeral reflect (the mediator acks no ephemeral reflect).
#[tokio::test]
async fn a_typing_indicator_that_owes_an_ack_is_acked_after_its_ephemeral_reflect_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(9_508).with_peer(ANN);
    let m = s.world.mediator().clone();
    let mut ann = s.world.peer(ANN);
    let warns = Warns::install_info();
    let mut h = harness::start(s);
    h.send(Command::Connect).await;
    let conn = h.wait_connected().await;
    h.ack_until("Online as leader", online_as("leader")).await;
    h.net().act(|_| {
        ann.send(
            &Letter::new(ME, 0x61, ty::TYPING_INDICATOR, &body::typing(1))
                .flags(flag::NO_SERVER_QUEUING),
        );
        ann.send(
            &Letter::new(ME, 0x62, ty::TYPING_INDICATOR, &body::typing(0))
                .flags(flag::NO_SERVER_QUEUING | flag::NO_SERVER_ACK),
        );
    });
    wait_reflects(&h, 2).await;
    wait_acks(&h, conn, 1).await;
    h.sync().await;
    assert!(
        m.reflects().iter().all(|r| r.flags & EPHEMERAL != 0),
        "a typing indicator was reflected for keeps"
    );
    assert_eq!(m.csp_queue_len(ME), 0, "the acked typing stayed queued");
    assert!(h.net().close_device(DEVICE, 4000), "no socket to close");
    let end = wait_lines(&warns, 1, count_line("end")).await;
    let counted = counters(&end[0]);
    assert_eq!(
        (
            counted["in_csp_message"],
            counted["out_ephemeral"],
            counted["out_reflect_inbound"],
            counted["out_message_ack"],
        ),
        (2, 2, 0, 1),
        "the leader's frames"
    );
    assert_eq!(
        (end[0].field("held"), end[0].field("held_by")),
        (Some("0"), Some("none")),
        "a hold"
    );
    equals_the_trace(&h, conn, &end[0]);
    no_id(&warns);
    h.finish().await;
}

#[test]
fn only_the_first_unanswered_reflect_of_a_batch_cools() {
    support::offline::run_child(
        module_path!(),
        "only_the_first_unanswered_reflect_of_a_batch_cools_child",
    );
}

/// TH6 FIX-B, P-L5 (critic T-M4 cools the suspect, not the bystanders): three texts of ANN are one batch. The mediator
/// answers the first reflect and then goes silent (it never answers the second, and so none behind it: the answers of a
/// connection come in order). The first text is reflected and acked. Both sockets (+60 s, +125 s) close with the second
/// and the third reflect unanswered; only the SECOND (the first one without its ack) counts toward the cooling. On the
/// third socket (the mediator answers again) the second is HELD (`inbound-reflect-unanswered`), while the third is
/// resumed with its stored bytes, reflected and acked. (Before the fix the third cooled with the second: nothing was
/// reflected and the third socket acked nothing.)
#[tokio::test]
async fn only_the_first_unanswered_reflect_of_a_batch_cools_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(9_509).with_peer(ANN);
    let m = s.world.mediator().clone();
    m.knobs(|k| k.withhold_reflect_acks = true);
    let mut ann = s.world.peer(ANN);
    for id in [0x71_u64, 0x72, 0x73] {
        ann.send(&Letter::text(ME, id, "synthetic text"));
    }
    let warns = Warns::install_info();
    let mut h = harness::start(s);
    h.send(Command::Connect).await;
    let first = h.wait_connected().await;
    wait_reflects(&h, 3).await;
    assert_eq!(
        h.net().act(|m| m.release_first_reflect_acks(first, 1)),
        1,
        "the first answer"
    );
    wait_acks(&h, first, 1).await;
    through_echoes(&h, at(0), at(60)).await;
    h.wait_state("Offline with a retry", retry_at(at(65))).await;
    h.advance_to(at(65)).await;
    h.wait_opened(2).await;
    let second = h.net().opened()[1];
    wait_reflects(&h, 5).await;
    through_echoes(&h, at(65), at(125)).await;
    h.wait_state("Offline with a retry", retry_at(at(130)))
        .await;
    // The mediator answers again from the third socket on.
    m.knobs(|k| k.withhold_reflect_acks = false);
    h.advance_to(at(130)).await;
    h.wait_opened(3).await;
    let third = h.wait_connected().await;
    wait_acks(&h, third, 1).await;
    wait_reflects(&h, 6).await;
    let cooled =
        |l: &Line| l.message == HELD && l.field("rule") == Some("inbound-reflect-unanswered");
    wait_lines(&warns, 1, cooled).await;
    assert_eq!(m.csp_queue_len(ME), 1, "only the cooled text stays queued");
    assert_eq!(
        traced(&h.s.world, second, "in", "csp-incoming-message-ack"),
        0,
        "the second socket acked"
    );
    // Nothing was sealed again: the second socket reflected the same two bytes, the third one of them.
    let reflects = m.reflects();
    let mut early: Vec<&[u8]> = reflects[1..3].iter().map(|r| r.bytes.as_slice()).collect();
    let mut resumed: Vec<&[u8]> = reflects[3..5].iter().map(|r| r.bytes.as_slice()).collect();
    early.sort_unstable();
    resumed.sort_unstable();
    assert_eq!(early, resumed, "a resumed reflect was sealed again");
    assert!(
        early.contains(&reflects[5].bytes.as_slice()),
        "the third socket reflected something new"
    );
    assert!(h.net().close_device(DEVICE, 4000), "no socket to close");
    let ends = wait_lines(&warns, 3, count_line("end")).await;
    let want = [
        (first, 3, 1, "0", "none"),
        (second, 2, 0, "0", "none"),
        (third, 1, 1, "1", "inbound-reflect-unanswered:1"),
    ];
    for (end, (conn, reflected, acked, held, held_by)) in ends.iter().zip(want) {
        let counted = counts_of(end);
        assert_eq!(
            (
                counted["out_reflect_inbound"],
                counted["out_message_ack"],
                end.field("held"),
                end.field("held_by")
            ),
            (reflected, acked, Some(held), Some(held_by)),
            "the socket's frames and holds"
        );
        equals_the_trace(&h, conn, end);
    }
    no_id(&warns);
    h.finish().await;
}

#[test]
fn an_unknown_type_in_a_batch_holds_the_group_text_behind_it_in_the_same_plan() {
    support::offline::run_child(
        module_path!(),
        "an_unknown_type_in_a_batch_holds_the_group_text_behind_it_in_the_same_plan_child",
    );
}

/// TH6 re-check R-L3 (ITS MUTANT, `plan_one`'s mark for a type the pinned enum lacks removed, dies here): ONE batch of two
/// boxes of stored contacts: ANN's message of a type the pinned enum lacks (a newer protocol's, maybe a group control the
/// steps cannot place), then BEN's text in CAT's group, which the file does not hold. Both are HELD in the same plan
/// (`csp-unknown-type`, `group-held-before`): without the mark set AT ONCE, BEN's text would be discarded
/// `group-unknown` and acked (and a `group-sync-request` job written) in the very batch whose first box was held blind:
/// lost for every device if that type made the group. The batch is made deterministic: a text of ANN first (its batch
/// waits for its reflect-ack, held by the mediator), so the two boxes wait in the FIFO until the actor is parked, and
/// the release makes them ONE batch. Nothing but the first text is acked, no job but its receipt is written, the server keeps both.
#[tokio::test]
async fn an_unknown_type_in_a_batch_holds_the_group_text_behind_it_in_the_same_plan_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(9_510).with_peer(ANN).with_peer(BEN).with_peer(CAT);
    let m = s.world.mediator().clone();
    m.knobs(|k| k.withhold_reflect_acks = true);
    let mut ann = s.world.peer(ANN);
    let mut ben = s.world.peer(BEN);
    ann.send(&Letter::text(ME, 0x81, "synthetic text"));
    ann.send(&Letter::new(
        ME,
        0x82,
        ty::UNKNOWN,
        b"synthetic body of a newer type",
    ));
    ben.send(&Letter::new(
        ME,
        0x83,
        ty::GROUP_TEXT,
        &body::group_member_container(CAT, CATS, &body::text("synthetic group text")),
    ));
    assert_eq!(m.csp_queue_len(ME), 3);
    let warns = Warns::install_info();
    let h = harness::start(s);
    h.send(Command::Connect).await;
    let first = h.wait_connected().await;
    wait_reflects(&h, 1).await;
    // The actor idles with the first text's batch in flight: the two boxes are in its FIFO.
    h.wait_parked().await;
    assert_eq!(
        h.net().act(|m| m.release_reflect_acks(first)),
        1,
        "the first text's reflect-ack"
    );
    wait_acks(&h, first, 1).await;
    let unknown = |l: &Line| l.message == HELD && l.field("rule") == Some("csp-unknown-type");
    let behind = |l: &Line| l.message == HELD && l.field("rule") == Some("group-held-before");
    wait_lines(&warns, 1, unknown).await;
    wait_lines(&warns, 1, behind).await;
    h.sync().await;
    assert_eq!(
        traced(&h.s.world, first, "in", "csp-incoming-message-ack"),
        1,
        "a held box was acked"
    );
    assert_eq!(m.csp_queue_len(ME), 2, "the server lost a held box");
    // The first text's receipt is the one job (a `group-sync-request` to CAT would be a second).
    assert_eq!(
        h.file_rows("send_task"),
        1,
        "a job was written for a held box"
    );
    assert!(h.net().close_device(DEVICE, 4000), "no socket to close");
    let end = wait_lines(&warns, 1, count_line("end")).await;
    let counted = counts_of(&end[0]);
    assert_eq!(
        (
            counted["in_csp_message"],
            counted["out_reflect_inbound"],
            counted["out_message_ack"]
        ),
        (3, 1, 1),
        "the leader's frames"
    );
    assert_eq!(
        (end[0].field("held"), end[0].field("held_by")),
        (Some("2"), Some("csp-unknown-type:1,group-held-before:1")),
        "the holds"
    );
    equals_the_trace(&h, first, &end[0]);
    no_id(&warns);
    h.finish().await;
}
