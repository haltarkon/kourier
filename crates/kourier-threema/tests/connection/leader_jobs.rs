//! TH6 L6: JOBS AND HOLDS against the offline fake (brief §2.11 with the critic's T-B1, T-H1, T-M1, T-M3, T-L6 and
//! T-L8). Kourier leads with the phone offline. Each job the receive steps send is a row of the send queue written in
//! the zone that makes it true, and each OPENS at its peer once the account is `Online`: the creator's three answers
//! to a member's `group-sync-request` (C7, only for a group whose picture is explicitly removed: T-M3 holds the other),
//! the empty setup to a non-member, the `group-sync-request` to a creator (once an hour per group), the `group-leave`
//! of a group the user left, the FS `Reject` to a stored contact (C8; a stranger's FS message is discarded without one,
//! the loss told). 300 texts of three peers make at most 30 coalesced receipts (C3). A full job budget HOLDS the
//! message whose job it refuses and leaves a reflected row `pending`: no close, no ack (S-L2). A REAL group change is
//! held for the phone (T-M1: an unchanged re-send is discarded and acked), and a new member's text in that group is
//! held at step 22.4.1 although the member is no contact (T-B1), while a known member's text is processed. With a hold
//! for the phone standing, Kourier steps aside ONCE when the phone comes online (T-H1), the phone leads and takes the
//! held message, and Kourier does not step aside again within 15 minutes. Every test ends with the guard oracle and the
//! violations check (`finish`).

use std::time::Duration;

use kourier_backend_api::Command;
use kourier_db::rusqlite::params;
use kourier_threema::failpoints as fp;
use libthreema::protobuf::common;
use libthreema::protobuf::d2d_sync;
use prost::Message as _;

use super::harness::{self, DEVICE, H, ME, SERVER_GROUP, scene, traced};
use super::lines::{
    WALL, at, count_line, counts_of, equals_the_trace, no_id, online_as, retry_at, wait_lines,
};
use super::reflect::PHONE;
use crate::support;
use crate::support::mock::START_MS;
use crate::support::mock::body::{self, ty};
use crate::support::mock::d2m::ConnId;
use crate::support::mock::envelope::GroupDelta;
use crate::support::mock::peer::{Letter, Opened};
use crate::support::mock::phone::LedOutcome;
use crate::support::warns::{Line, Warns};

/// Stored contacts with real keys (world identities).
const ANN: &str = "PEER0001";
const BEN: &str = "PEER0002";
const CAT: &str = "PEER0003";
/// A world identity the file does not hold (the directory knows it).
const DAN: &str = "PEER0004";
/// Another world identity the file does not hold.
const EVE: &str = "PEER0005";
/// The user's group whose stored picture is EXPLICITLY removed, ANN a member.
const OWN_PIC: u64 = 0x1111_0000_0000_0001;
/// The user's group whose stored picture was never stated (T-M3), ANN a member.
const OWN_NONE: u64 = 0x1111_0000_0000_0002;
/// ANN's group, BEN a member.
const ANNS: u64 = 0x2222_0000_0000_0001;
/// BEN's group with ANN, which the user LEFT.
const LEFT: u64 = 0x3333_0000_0000_0001;
/// D2's words.
const HELD: &str = "a chat server message is held for the phone";
/// C8's loss, told.
const FS_LOST: &str = "a forward-security message of a sender who is no contact was discarded without a reject; it is lost for every device";
/// T-H1's line.
const STEP_ASIDE: &str =
    "messages are held for the phone and another device is online; stepping aside so it can lead";
/// The upper bound of the backlog's wait (300 messages through the real steps and zones in a debug build).
const BACKLOG_WALL: Duration = Duration::from_secs(90);

/// Waits (a wall BOUND) until `conn` sent `n` CSP acks of incoming messages, and not more.
async fn wait_acks(h: &H, conn: ConnId, n: usize) {
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

/// Waits (a wall BOUND) until `conn` sent `n` boxes (the jobs), and not more.
async fn wait_boxes(h: &H, conn: ConnId, n: usize) {
    let world = &h.s.world;
    assert!(
        h.net()
            .wait(
                || traced(world, conn, "in", "csp-outgoing-message") >= n,
                WALL
            )
            .await,
        "{} of {n} jobs went",
        traced(world, conn, "in", "csp-outgoing-message")
    );
    assert_eq!(
        traced(world, conn, "in", "csp-outgoing-message"),
        n,
        "more jobs than expected"
    );
}

/// The stored group (`creator`, `group_id`) replaced by `group`, as a sync of the phone would have left it.
fn restate_group(h: &H, creator: &str, group_id: u64, group: &d2d_sync::Group) {
    h.db()
        .zone(|tx| {
            let n = tx.execute(
                "UPDATE grp SET sync = ?1 WHERE creator = ?2 AND group_id = ?3",
                params![
                    group.encode_to_vec(),
                    creator,
                    i64::from_ne_bytes(group_id.to_ne_bytes())
                ],
            )?;
            assert_eq!(n, 1, "the group was not stored");
            Ok::<(), kourier_db::Error>(())
        })
        .expect("restate the group");
}

/// Whether the file holds the contact `id`.
fn contact_stored(h: &H, id: &str) -> bool {
    h.db()
        .read(|conn: &kourier_db::rusqlite::Connection| {
            conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM contact WHERE identity = ?1)",
                [id],
                |r| r.get::<_, bool>(0),
            )
            .map_err(kourier_db::Error::from)
        })
        .expect("look up the contact")
}

/// What a peer opened, as (type, body), sorted.
fn sorted(opened: &[Opened]) -> Vec<(u8, Vec<u8>)> {
    let mut all: Vec<(u8, Vec<u8>)> = opened.iter().map(|o| (o.ty, o.body.clone())).collect();
    all.sort();
    all
}

/// An FS envelope with `encapsulated` (`csp-e2e-fs.proto`: session id = 1, encapsulated = 6), written by hand.
fn fs_encapsulated() -> Vec<u8> {
    let mut out = vec![0x0a, 16];
    out.extend_from_slice(&[0x5a; 16]);
    out.extend_from_slice(&[0x32, 2, 0x10, 0x01]);
    out
}

/// The `Reject { message_id, DISABLED_BY_LOCAL }` under the same session, as `csp-e2e-fs.proto` writes it.
fn fs_reject(message_id: u64) -> Vec<u8> {
    let mut out = vec![0x0a, 16];
    out.extend_from_slice(&[0x5a; 16]);
    out.extend_from_slice(&[0x22, 11, 0x09]);
    out.extend_from_slice(&message_id.to_le_bytes());
    out.extend_from_slice(&[0x10, 2]);
    out
}

/// `common.GroupIdentity` by hand: `group_id` `fixed64` 1, `creator_identity` string 2 (19 bytes).
fn group_identity(creator: &str, group_id: u64) -> Vec<u8> {
    let mut out = vec![0x09];
    out.extend_from_slice(&group_id.to_le_bytes());
    out.extend_from_slice(&[0x12, 8]);
    out.extend_from_slice(creator.as_bytes());
    out
}

/// An FS envelope whose `Encapsulated` names a group (P-L4: `Encapsulated.group_identity` = 6), written by hand.
fn fs_encapsulated_group(creator: &str, group_id: u64) -> Vec<u8> {
    let identity = group_identity(creator, group_id);
    let mut out = vec![0x0a, 16];
    out.extend_from_slice(&[0x5a; 16]);
    out.extend_from_slice(&[0x32, 2 + 19, 0x32, 19]);
    out.extend_from_slice(&identity);
    out
}

/// The `Reject` of that group message: the 1:1 form and `group_identity` = 3 after the cause (P-L4, `:587`).
fn fs_group_reject(message_id: u64, creator: &str, group_id: u64) -> Vec<u8> {
    let identity = group_identity(creator, group_id);
    let mut out = vec![0x0a, 16];
    out.extend_from_slice(&[0x5a; 16]);
    out.extend_from_slice(&[0x22, 11 + 2 + 19, 0x09]);
    out.extend_from_slice(&message_id.to_le_bytes());
    out.extend_from_slice(&[0x10, 2, 0x1a, 19]);
    out.extend_from_slice(&identity);
    out
}

/// A group text of `creator`'s group `group_id`.
fn group_text(id: u64, creator: &str, group_id: u64) -> Letter {
    Letter::new(
        ME,
        id,
        ty::GROUP_TEXT,
        &body::group_member_container(creator, group_id, &body::text("synthetic group text")),
    )
}

/// A `group-sync-request` for the user's group `group_id`.
fn sync_request(id: u64, group_id: u64) -> Letter {
    Letter::new(
        ME,
        id,
        ty::GROUP_SYNC_REQUEST,
        &body::group_member_container(ME, group_id, &[]),
    )
}

/// ANN's `group-setup` of ANNS with `members`.
fn setup(id: u64, members: &[&str]) -> Letter {
    Letter::new(
        ME,
        id,
        ty::GROUP_SETUP,
        &body::group_creator_container(ANNS, &body::group_setup(members)),
    )
}

#[test]
fn each_job_opens_at_its_peer_and_an_fs_message_gets_a_reject() {
    support::offline::run_child(
        module_path!(),
        "each_job_opens_at_its_peer_and_an_fs_message_gets_a_reject_child",
    );
}

/// THE JOBS (brief §2.11): ten messages wait while the phone is offline. ANN asks for the user's group with a removed
/// picture: kept (reflected, acked) and answered with setup, name and delete-picture (C7). CAT, no member, asks for it:
/// kept and answered with an empty setup. ANN asks for the user's group whose picture was never stated: HELD
/// `creator-answer-needs-picture` (T-M3), nothing sent. CAT writes twice into ANN's group (no member): both discarded
/// and acked, ONE `group-sync-request` to ANN (the hour's throttle). BEN writes into the user's group (no member): an
/// empty setup to BEN. ANN writes into BEN's group the user left: a `group-leave` to ANN. ANN's encapsulated FS message:
/// discarded, acked, a `Reject { DISABLED_BY_LOCAL }` to ANN (C8). EVE's (no contact, the directory knows her): discarded
/// and acked WITHOUT a reject, the loss told once (T-L8). BEN's encapsulated message of ANN's GROUP: discarded, acked,
/// and the reject names the group (P-L4). Once `Online`, each job's box opens at its peer: exactly
/// these, nothing to EVE. The end line equals the trace.
#[tokio::test]
async fn each_job_opens_at_its_peer_and_an_fs_message_gets_a_reject_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(9_701)
        .with_peer(CAT)
        .with_members(ME, OWN_PIC, &[ANN])
        .with_members(ME, OWN_NONE, &[ANN])
        .with_members(ANN, ANNS, &[ANN, BEN])
        .with_members(BEN, LEFT, &[BEN, ANN]);
    s.world.add_identity(EVE);
    let mut ann = s.world.peer(ANN);
    let mut ben = s.world.peer(BEN);
    let mut cat = s.world.peer(CAT);
    let mut eve = s.world.peer(EVE);
    ann.send(&sync_request(0x41, OWN_PIC));
    cat.send(&sync_request(0x42, OWN_PIC));
    ann.send(&sync_request(0x43, OWN_NONE));
    cat.send(&group_text(0x44, ANN, ANNS).discarded());
    ben.send(&group_text(0x45, ME, OWN_PIC).discarded());
    ann.send(&group_text(0x46, BEN, LEFT).discarded());
    cat.send(&group_text(0x47, ANN, ANNS).discarded());
    ann.send(&Letter::new(ME, 0x48, ty::FORWARD_SECURITY_ENVELOPE, &fs_encapsulated()).discarded());
    eve.send(&Letter::new(ME, 0x49, ty::FORWARD_SECURITY_ENVELOPE, &fs_encapsulated()).discarded());
    // P-L4: BEN's encapsulated message of ANN's group: the reject names the group.
    ben.send(
        &Letter::new(
            ME,
            0x4a,
            ty::FORWARD_SECURITY_ENVELOPE,
            &fs_encapsulated_group(ANN, ANNS),
        )
        .discarded(),
    );
    let mut phone = s.world.phone(&s.dgk, SERVER_GROUP, PHONE);
    phone.connect();
    phone.disconnect();
    let warns = Warns::install_info();
    let mut h = harness::start(s);
    h.sync().await;
    let removed = common::DeltaImage {
        image: Some(common::delta_image::Image::Removed(common::Unit {})),
    };
    let mut own_pic = GroupDelta::create(ME, OWN_PIC, "synthetic group", &[ANN], START_MS).build();
    own_pic.profile_picture = Some(removed);
    restate_group(&h, ME, OWN_PIC, &own_pic);
    let left = GroupDelta::create(BEN, LEFT, "synthetic group", &[BEN, ANN], START_MS)
        .user_state(d2d_sync::group::UserState::Left)
        .build();
    restate_group(&h, BEN, LEFT, &left);
    h.send(Command::Connect).await;
    let conn = h.wait_connected().await;
    wait_acks(&h, conn, 9).await;
    h.ack_until("Online as leader", online_as("leader")).await;
    wait_boxes(&h, conn, 9).await;
    let in_own = |inner: &[u8]| body::group_creator_container(OWN_PIC, inner);
    let mut want_ann = vec![
        (ty::GROUP_SETUP, in_own(&body::group_setup(&[ANN]))),
        (ty::GROUP_NAME, in_own(&body::group_name("synthetic group"))),
        (ty::GROUP_DELETE_PROFILE_PICTURE, in_own(&[])),
        (
            ty::GROUP_SYNC_REQUEST,
            body::group_member_container(ANN, ANNS, &[]),
        ),
        (
            ty::GROUP_LEAVE,
            body::group_member_container(BEN, LEFT, &[]),
        ),
        (ty::FORWARD_SECURITY_ENVELOPE, fs_reject(0x48)),
    ];
    want_ann.sort();
    assert_eq!(sorted(&ann.drain()), want_ann, "what ANN opened");
    let empty_setup = vec![(ty::GROUP_SETUP, in_own(&body::group_setup(&[])))];
    assert_eq!(sorted(&cat.drain()), empty_setup, "what CAT opened");
    let mut want_ben = vec![
        (ty::GROUP_SETUP, in_own(&body::group_setup(&[]))),
        (
            ty::FORWARD_SECURITY_ENVELOPE,
            fs_group_reject(0x4a, ANN, ANNS),
        ),
    ];
    want_ben.sort();
    assert_eq!(sorted(&ben.drain()), want_ben, "what BEN opened");
    assert!(eve.drain().is_empty(), "EVE got a reject");
    assert_eq!(
        warns
            .lines()
            .iter()
            .filter(|l| l.message == FS_LOST)
            .count(),
        1,
        "the FS loss was not told once"
    );
    assert_eq!(
        h.s.world.mediator().csp_queue_len(ME),
        1,
        "the held request"
    );
    assert!(h.net().close_device(DEVICE, 4000), "no socket to close");
    let end = wait_lines(&warns, 1, count_line("end")).await;
    assert_eq!(
        (end[0].field("held"), end[0].field("held_by")),
        (Some("1"), Some("creator-answer-needs-picture:1")),
        "the held request"
    );
    let counted = counts_of(&end[0]);
    assert_eq!(
        (
            counted["in_csp_message"],
            counted["out_reflect_inbound"],
            counted["out_message_ack"],
            counted["out_box_job"],
        ),
        (10, 2, 9, 9),
        "the leader's frames"
    );
    equals_the_trace(&h, conn, &end[0]);
    no_id(&warns);
    h.finish().await;
}

#[test]
fn three_hundred_texts_of_three_peers_make_at_most_thirty_receipts() {
    support::offline::run_child(
        module_path!(),
        "three_hundred_texts_of_three_peers_make_at_most_thirty_receipts_child",
    );
}

/// THE BURST (call C3): 300 texts, 100 of each of three peers in turn, wait while the phone is offline. The first socket
/// takes 286 (the FIFO's bound, critic T-H2) and the second the rest; every ZONE REFLECTED writes ONE receipt job per
/// sender, so the file holds at most 30 receipt jobs (one per message would be 300) once every text is acked. At
/// `Online` they go: each peer opens receipts of status 1 that name each of its 100 ids exactly once, at most 30 ids
/// each. (The peers send in turn: interleaved, the eleven zones the FIFO's bound makes would allow 33.)
#[tokio::test]
async fn three_hundred_texts_of_three_peers_make_at_most_thirty_receipts_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(9_702).with_peer(ANN).with_peer(BEN).with_peer(CAT);
    let mut peers = [s.world.peer(ANN), s.world.peer(BEN), s.world.peer(CAT)];
    for (p, peer) in peers.iter_mut().enumerate() {
        for n in 0..100_u64 {
            let id = 0x1_0000 * (u64::try_from(p).expect("fits") + 1) + n;
            peer.send(&Letter::text(ME, id, "synthetic backlog"));
        }
    }
    let m = s.world.mediator().clone();
    assert_eq!(m.csp_queue_len(ME), 300);
    let mut h = harness::start(s);
    h.send(Command::Connect).await;
    assert!(
        h.net()
            .wait(|| m.csp_queue_len(ME) == 0, BACKLOG_WALL)
            .await,
        "{} texts still queued",
        m.csp_queue_len(ME)
    );
    h.sync().await;
    // Every receipt job is written (ZONE REFLECTED precedes each ack) and none ran (no `Online` before the core acks).
    let jobs = usize::try_from(h.file_rows("send_task")).expect("fits");
    assert!(
        (3..=30).contains(&jobs),
        "{jobs} receipt jobs for 300 texts"
    );
    let opened = h.net().opened();
    assert_eq!(opened.len(), 2, "not two sockets");
    h.ack_until("Online as leader", online_as("leader")).await;
    wait_boxes(&h, opened[1], jobs).await;
    for (p, peer) in peers.iter_mut().enumerate() {
        let mut ids = Vec::new();
        for receipt in peer.drain() {
            assert_eq!(receipt.ty, ty::DELIVERY_RECEIPT, "not a receipt");
            assert_eq!(receipt.body[0], 1, "not `received`");
            let named = &receipt.body[1..];
            assert!(
                named.len() % 8 == 0 && (1..=30).contains(&(named.len() / 8)),
                "a receipt of {} bytes of ids",
                named.len()
            );
            ids.extend(
                named
                    .chunks(8)
                    .map(|c| u64::from_le_bytes(c.try_into().expect("8 bytes"))),
            );
        }
        let base = 0x1_0000 * (u64::try_from(p).expect("fits") + 1);
        let want: Vec<u64> = (base..base + 100).collect();
        ids.sort_unstable();
        assert_eq!(ids, want, "the ids peer {p}'s receipts name");
    }
    h.finish().await;
}

#[test]
fn a_full_job_budget_holds_the_message_without_a_close_or_an_ack() {
    support::offline::run_child(
        module_path!(),
        "a_full_job_budget_holds_the_message_without_a_close_or_an_ack_child",
    );
}

/// S-L2 (MUTANT M6, `QueueFull` through the H3 close, dies here): the job budget is spent (2,000 open job rows) and the
/// core never acks, so the account stays `Syncing` and no job runs. ANN's text is reflected, but ZONE REFLECTED cannot
/// write its receipt: the row stays `pending`, NO ack, HELD and counted `inbound-reflected-queue-full` (TH6 FIX-A,
/// S-L1). CAT's text into ANN's group (no member) needs a `group-sync-request`: its RECEIVE zone is refused, the message
/// HELD `inbound-queue-full` (the batch redone one message per zone, T-L6), NO ack. The socket stays: no close, no
/// backoff, nothing acked.
#[tokio::test]
async fn a_full_job_budget_holds_the_message_without_a_close_or_an_ack_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(9_703)
        .with_peer(CAT)
        .with_members(ANN, ANNS, &[ANN, BEN]);
    let mut ann = s.world.peer(ANN);
    let mut cat = s.world.peer(CAT);
    ann.send(&Letter::text(ME, 0x51, "synthetic text"));
    cat.send(&group_text(0x52, ANN, ANNS));
    let mut phone = s.world.phone(&s.dgk, SERVER_GROUP, PHONE);
    phone.connect();
    phone.disconnect();
    let warns = Warns::install_info();
    let h = harness::start(s);
    h.sync().await;
    h.db()
        .zone(|tx| {
            tx.execute(
                "WITH RECURSIVE k(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM k WHERE i < 2000) \
                 INSERT INTO send_task (origin, kind, local, message_id, conversation, created_at, state, \
                   has_update, bytes, queued_at) \
                 SELECT 'job', 'seed', NULL, i, X'0A', 0, 'reflected', 0, 0, ?1 FROM k",
                params![i64::try_from(START_MS).expect("fits")],
            )?;
            Ok::<(), kourier_db::Error>(())
        })
        .expect("spend the job budget");
    h.send(Command::Connect).await;
    let conn = h.wait_connected().await;
    let held = |l: &Line| l.message == HELD && l.field("rule") == Some("inbound-queue-full");
    wait_lines(&warns, 1, held).await;
    let pending =
        |l: &Line| l.message == "received rows left pending: 1 by inbound-reflected-queue-full";
    wait_lines(&warns, 1, pending).await;
    h.sync().await;
    let m = h.s.world.mediator().clone();
    assert_eq!(m.reflects().len(), 1, "not ANN's one reflect");
    assert_eq!(
        traced(&h.s.world, conn, "in", "csp-incoming-message-ack"),
        0,
        "an ack under a spent budget"
    );
    assert_eq!(m.closed(conn), None, "the socket closed");
    assert_eq!(h.net().opened().len(), 1, "a second socket");
    assert_eq!(m.csp_queue_len(ME), 2, "a message left the server");
    assert_eq!(
        h.file_rows("send_task"),
        2_000,
        "a job was written over the budget"
    );
    assert!(h.net().close_device(DEVICE, 4000), "no socket to close");
    let end = wait_lines(&warns, 1, count_line("end")).await;
    assert_eq!(
        (end[0].field("held"), end[0].field("held_by")),
        (
            Some("2"),
            Some("inbound-queue-full:1,inbound-reflected-queue-full:1")
        ),
        "the held message and the pending row"
    );
    equals_the_trace(&h, conn, &end[0]);
    no_id(&warns);
    h.finish().await;
}

#[test]
fn a_held_group_setup_holds_a_new_members_text_and_lets_a_known_members_through() {
    support::offline::run_child(
        module_path!(),
        "a_held_group_setup_holds_a_new_members_text_and_lets_a_known_members_through_child",
    );
}

/// HIS TH6-Q1 "They wait for your phone", critic T-M1 and T-B1 (MUTANT M2, a held setup acked, and T-B1's, the new
/// member's text acked at 22.4.1, die here): ANN re-sends her group's unchanged setup (discarded and acked, nothing
/// held), then ADDS DAN, whom the file does not hold (a real change: HELD `group-sync-needed`). DAN's text in the group
/// is looked up (the directory knows him) and HELD `group-held-before` at 22.4.1: not discarded, not acked, no contact
/// made. BEN's text (a stored member) is reflected and acked. Nothing is acked unreflected (the oracle); both held
/// messages stay at the server.
#[tokio::test]
async fn a_held_group_setup_holds_a_new_members_text_and_lets_a_known_members_through_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(9_704).with_members(ANN, ANNS, &[ANN, BEN]);
    s.world.add_identity(DAN);
    let mut ann = s.world.peer(ANN);
    let mut ben = s.world.peer(BEN);
    let mut dan = s.world.peer(DAN);
    ann.send(&setup(0x61, &[ME, BEN]).discarded());
    ann.send(&setup(0x62, &[ME, BEN, DAN]));
    dan.send(&group_text(0x63, ANN, ANNS));
    ben.send(&group_text(0x64, ANN, ANNS));
    let mut phone = s.world.phone(&s.dgk, SERVER_GROUP, PHONE);
    phone.connect();
    phone.disconnect();
    let warns = Warns::install_info();
    let h = harness::start(s);
    assert!(!contact_stored(&h, DAN), "DAN is a contact before");
    h.send(Command::Connect).await;
    let conn = h.wait_connected().await;
    wait_acks(&h, conn, 2).await;
    let rules = |l: &Line| l.message == HELD;
    let held = wait_lines(&warns, 2, rules).await;
    let held: Vec<&str> = held.iter().filter_map(|l| l.field("rule")).collect();
    assert_eq!(
        held,
        ["group-sync-needed", "group-held-before"],
        "the holds in order"
    );
    h.sync().await;
    let m = h.s.world.mediator().clone();
    assert_eq!(m.reflects().len(), 1, "not BEN's one reflect");
    assert_eq!(m.csp_queue_len(ME), 2, "the held two left the server");
    assert!(!contact_stored(&h, DAN), "DAN became a contact");
    let asked = h.net().directory().asked();
    assert_eq!(asked.len(), 1, "not one lookup");
    assert_eq!(asked[0].ids, [DAN], "the lookup's IDs");
    assert!(h.net().close_device(DEVICE, 4000), "no socket to close");
    let end = wait_lines(&warns, 1, count_line("end")).await;
    assert_eq!(
        (end[0].field("held"), end[0].field("held_by")),
        (Some("2"), Some("group-held-before:1,group-sync-needed:1")),
        "the held messages"
    );
    equals_the_trace(&h, conn, &end[0]);
    // The phone (back while Kourier waits for its next socket) views BEN's text alone.
    phone.connect();
    phone.incoming();
    assert_eq!(
        phone.view().incoming_ids(),
        [(BEN.to_owned(), 0x64)],
        "the phone's view"
    );
    phone.disconnect();
    no_id(&warns);
    h.finish().await;
}

/// How often `conn` asked the mediator for the devices.
fn asked(h: &H, conn: ConnId) -> usize {
    traced(&h.s.world, conn, "in", "get-devices-info")
}

/// Moves both clocks from one deadline of the actor to the next until `done` holds (a bound of 400 looks). The next
/// deadline is read once the actor PARKED (`H::wait_parked`): after a plain `sync` the list can be stale, because the
/// actor's command loop is polled before its connection task (the post-rebase fix's lesson).
async fn advance_until(h: &H, what: &str, done: impl Fn() -> bool) {
    for _ in 0..400 {
        h.sync().await;
        if done() {
            return;
        }
        let deadlines = h.wait_parked().await;
        if done() {
            return;
        }
        h.advance_to(deadlines[0].max(h.now())).await;
    }
    h.sync().await;
    assert!(
        done(),
        "{what} never came: at +{} ms, deadlines {:?}, {} sockets, {:?}",
        h.now().saturating_sub(at(0)),
        h.clock
            .deadlines()
            .iter()
            .map(|d| d.saturating_sub(at(0)))
            .collect::<Vec<_>>(),
        h.net().opened().len(),
        h.state()
    );
}

#[test]
fn with_a_hold_for_the_phone_kourier_steps_aside_once_when_the_phone_comes_online() {
    support::offline::run_child(
        module_path!(),
        "with_a_hold_for_the_phone_kourier_steps_aside_once_when_the_phone_comes_online_child",
    );
}

/// Critic T-H1, THE STEP-ASIDE: ANN's real group change is held for the phone while the phone is offline; the idle
/// leader asks the mediator for the devices (nobody else is online: it stays). The phone comes online as a follower;
/// at the next ask (+60 s) Kourier closes 1000 ONCE (not counted: the next socket after two minutes), the mediator
/// promotes the phone, and the phone takes the held setup from the chat server. Kourier comes back a follower. The
/// phone leaves, Kourier leads again, holds ANN's next change and the phone comes back: Kourier does NOT step aside
/// before 15 minutes after the first time (no ask on that socket until then), and steps aside at +960 s.
#[tokio::test]
async fn with_a_hold_for_the_phone_kourier_steps_aside_once_when_the_phone_comes_online_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(9_705).with_members(ANN, ANNS, &[ANN, BEN]);
    let m = s.world.mediator().clone();
    // The phone sends nothing for minutes: the mediator's own idle timer must not close it.
    m.knobs(|k| k.idle_timeout = Duration::from_secs(4 * 3600));
    let mut ann = s.world.peer(ANN);
    ann.send(&setup(0x71, &[ME, BEN, CAT]));
    let mut phone = s.world.phone(&s.dgk, SERVER_GROUP, PHONE);
    phone.connect();
    phone.disconnect();
    let warns = Warns::install_info();
    let mut h = harness::start(s);
    h.send(Command::Connect).await;
    let first = h.wait_connected().await;
    assert!(
        h.net().wait(|| asked(&h, first) == 1, WALL).await,
        "the leader did not ask for the devices"
    );
    h.sync().await;
    assert_eq!(m.closed(first), None, "it stepped aside with nobody online");
    phone.connect();
    assert!(!phone.leads(), "the phone leads beside Kourier");
    advance_until(&h, "+60 s", || h.now() >= at(60)).await;
    assert_eq!(h.now(), at(60), "the next ask's moment");
    assert!(
        h.net().wait(|| m.closed(first).is_some(), WALL).await,
        "no step-aside at the next ask"
    );
    assert_eq!(m.closed(first), Some(1000), "not the client's clean close");
    assert_eq!(asked(&h, first), 2, "not two asks");
    h.wait_state("Offline for two minutes", retry_at(at(180)))
        .await;
    assert!(phone.leads(), "the phone was not promoted");
    let led = phone.lead_csp_queue(ME).expect("the phone leads");
    assert_eq!(led.len(), 1, "the phone did not take the held setup");
    assert!(
        matches!(led[0].outcome, LedOutcome::Reflected { .. }),
        "{:?}",
        led[0]
    );
    assert_eq!(m.csp_queue_len(ME), 0, "the setup is still queued");
    h.advance_to(at(180)).await;
    h.ack_until("Online as follower", online_as("follower"))
        .await;
    let second = h.conn();
    // The phone leaves: Kourier is promoted, unblocks and holds ANN's next change (the fake's frames to Kourier go out at
    // the flush: the phone and the peer act on the mediator directly).
    phone.disconnect();
    h.net().flush();
    h.ack_until("Online as leader", online_as("leader")).await;
    ann.send(&setup(0x72, &[ME, BEN, EVE]));
    h.net().flush();
    let changes = |l: &Line| l.message == HELD && l.field("rule") == Some("group-sync-needed");
    wait_lines(&warns, 2, changes).await;
    phone.connect();
    advance_until(&h, "+900 s", || h.now() >= at(900)).await;
    h.sync().await;
    assert_eq!(m.closed(second), None, "it stepped aside within 15 minutes");
    assert_eq!(asked(&h, second), 0, "it asked within 15 minutes");
    advance_until(&h, "+960 s", || h.now() >= at(960)).await;
    assert_eq!(
        h.now(),
        at(960),
        "the ask 15 minutes after the first step-aside"
    );
    assert!(
        h.net().wait(|| m.closed(second).is_some(), WALL).await,
        "no second step-aside 15 minutes after the first"
    );
    assert_eq!(m.closed(second), Some(1000), "not the client's clean close");
    assert_eq!(asked(&h, second), 1, "not one ask");
    assert!(phone.leads(), "the phone was not promoted again");
    let told = warns
        .lines()
        .iter()
        .filter(|l| l.message == STEP_ASIDE)
        .count();
    assert_eq!(told, 2, "not two step-aside lines");
    phone.disconnect();
    no_id(&warns);
    h.finish().await;
}

#[test]
fn a_creators_periodic_resync_of_a_group_without_a_picture_holds_nothing() {
    support::offline::run_child(
        module_path!(),
        "a_creators_periodic_resync_of_a_group_without_a_picture_holds_nothing_child",
    );
}

/// TH6 FIX-A, P-M1 (the DELETE half): ANN's weekly Periodic Group Sync of her group, which the file holds WITHOUT a
/// picture field (Desktop leaves it out for a group made without one): the unchanged `group-setup`, the unchanged
/// `group-name` and a `group-delete-profile-picture`. All three are no change: discarded and acked, nothing held,
/// nothing reflected, no job (before the fix the third was held `group-sync-needed` every week).
#[tokio::test]
async fn a_creators_periodic_resync_of_a_group_without_a_picture_holds_nothing_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(9_706).with_members(ANN, ANNS, &[ANN, BEN]);
    let mut ann = s.world.peer(ANN);
    ann.send(&setup(0x91, &[ME, BEN]).discarded());
    ann.send(
        &Letter::new(
            ME,
            0x92,
            ty::GROUP_NAME,
            &body::group_creator_container(ANNS, &body::group_name("synthetic group")),
        )
        .discarded(),
    );
    ann.send(
        &Letter::new(
            ME,
            0x93,
            ty::GROUP_DELETE_PROFILE_PICTURE,
            &body::group_creator_container(ANNS, &[]),
        )
        .discarded(),
    );
    let mut phone = s.world.phone(&s.dgk, SERVER_GROUP, PHONE);
    phone.connect();
    phone.disconnect();
    let warns = Warns::install_info();
    let mut h = harness::start(s);
    let picture = h
        .db()
        .read(|conn: &kourier_db::rusqlite::Connection| {
            conn.query_row("SELECT sync FROM grp WHERE creator = ?1", [ANN], |r| {
                r.get::<_, Vec<u8>>(0)
            })
            .map_err(kourier_db::Error::from)
        })
        .expect("read the group");
    assert!(
        d2d_sync::Group::decode(picture.as_slice())
            .expect("a group")
            .profile_picture
            .is_none(),
        "the stored group has a picture field"
    );
    h.send(Command::Connect).await;
    let conn = h.wait_connected().await;
    wait_acks(&h, conn, 3).await;
    h.ack_until("Online as leader", online_as("leader")).await;
    let m = h.s.world.mediator().clone();
    assert!(m.reflects().is_empty(), "a no-change message was reflected");
    assert_eq!(m.csp_queue_len(ME), 0, "a message stayed at the server");
    assert_eq!(h.file_rows("send_task"), 0, "a job was written");
    assert!(h.net().close_device(DEVICE, 4000), "no socket to close");
    let end = wait_lines(&warns, 1, count_line("end")).await;
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
fn a_reflected_zone_that_fails_leaves_the_row_pending_unacked_and_counted() {
    support::offline::run_child(
        module_path!(),
        "a_reflected_zone_that_fails_leaves_the_row_pending_unacked_and_counted_child",
    );
}

/// TH6 FIX-A, S-L1: ZONE REFLECTED fails by its content (the store's door, code 275): ANN's text was received and
/// reflected, its reflect-ack came, but its row stays `pending` and it is NOT acked; the socket counts it `held`
/// (`inbound-reflected-zone-failed`, a D2 line and the WARN), so the count line shows the un-acked message. The door off,
/// the next socket resumes the row (the stored reflect again) and acks it.
#[tokio::test]
async fn a_reflected_zone_that_fails_leaves_the_row_pending_unacked_and_counted_child() {
    if !support::offline::in_child() {
        return;
    }
    let s = scene(9_707).with_peer(ANN);
    let mut ann = s.world.peer(ANN);
    ann.send(&Letter::text(ME, 0xa1, "synthetic text"));
    let mut phone = s.world.phone(&s.dgk, SERVER_GROUP, PHONE);
    phone.connect();
    phone.disconnect();
    kourier_threema::store::fail_inbound_zone(fp::INSIDE_INBOUND_REFLECTED, 275);
    let warns = Warns::install_info();
    let mut h = harness::start(s);
    h.send(Command::Connect).await;
    let first = h.wait_connected().await;
    let rule = "inbound-reflected-zone-failed";
    wait_lines(&warns, 1, |l: &Line| {
        l.message == HELD && l.field("rule") == Some(rule)
    })
    .await;
    wait_lines(&warns, 1, |l: &Line| {
        l.message == "received rows left pending: 1 by inbound-reflected-zone-failed"
    })
    .await;
    h.sync().await;
    let m = h.s.world.mediator().clone();
    assert_eq!(m.reflects().len(), 1, "not the text's one reflect");
    assert_eq!(
        traced(&h.s.world, first, "in", "csp-incoming-message-ack"),
        0,
        "an ack of a row left pending"
    );
    assert_eq!(m.csp_queue_len(ME), 1, "the text left the server");
    assert_eq!(h.file_rows("csp_inbound"), 1, "not the pending row");
    kourier_threema::store::fail_inbound_zone("", 0);
    assert!(h.net().close_device(DEVICE, 4000), "no socket to close");
    let end = wait_lines(&warns, 1, count_line("end")).await;
    assert_eq!(
        (end[0].field("held"), end[0].field("held_by")),
        (Some("1"), Some("inbound-reflected-zone-failed:1")),
        "the un-acked row"
    );
    let counted = counts_of(&end[0]);
    assert_eq!(
        (counted["out_reflect_inbound"], counted["out_message_ack"]),
        (1, 0),
        "the first socket's reflect and acks"
    );
    equals_the_trace(&h, first, &end[0]);
    h.wait_state("Offline with a retry", retry_at(at(5))).await;
    h.advance_to(at(5)).await;
    h.wait_opened(2).await;
    let second = h.net().opened()[1];
    wait_acks(&h, second, 1).await;
    assert_eq!(m.csp_queue_len(ME), 0, "the text stayed at the server");
    assert_eq!(m.reflects().len(), 2, "not the resumed reflect");
    assert_eq!(
        m.reflects()[0].bytes,
        m.reflects()[1].bytes,
        "the resumed reflect was sealed again"
    );
    no_id(&warns);
    h.finish().await;
}
