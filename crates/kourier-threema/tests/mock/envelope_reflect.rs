//! The phone's builders through the fake mediator (TH5 A2): a sealed reflection opens with libthreema's
//! patch P5 (`DeviceGroupKey::open_reflected`, the cross-check of A1 against the fake's own seal) and
//! its plaintext decodes to the envelope that was built; a typing indicator is reflected ephemeral and a
//! typing WITHOUT the flag is stored and acked like any message (the flag decides the ack, M5);
//! `reflect_during_join` (B1) delivers right after the first `ServerInfo` of a NEW slot, before Dry, in
//! order; `on_reflected_ack` (L11) records the ids, their order and the virtual time, and its hook runs
//! with the mediator's lock released. One thread, no runtime, no unshare (as `reflect.rs`). A failure
//! says what differs and never prints a body, an ID or a key.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use libthreema::common::keys::DeviceGroupKey;
use libthreema::d2m::payload::{IncomingPayload, OutgoingPayload, Reflected, ReflectedAck};
use libthreema::protobuf::d2d;
use prost::Message as _;

use super::d2m::{Client, Group, SERVER_GROUP, existing, group, new_device, online, pump, send};
use crate::support::mock::body::{MsgId, id_u64};
use crate::support::mock::d2m::{AckSeen, EPHEMERAL};
use crate::support::mock::envelope::{
    Author, ContactDelta, Conv, GroupDelta, Msg, Reflect as _, flags_for, tampered, too_short,
};
use crate::support::mock::phone::{Phone, PhoneEvent};
use crate::support::mock::{MockWorld, START_MS, world};

const PHONE_ID: u64 = 0x0FF0_0000_0000_0001;
const KOURIER_ID: u64 = 0xF000_0000_0000_0002;
const OTHER_ID: u64 = 0x0000_0000_0000_0003;
const THIRD_ID: u64 = 0x0000_0000_0000_0004;

const PEER: &str = "PEERAAAA";
const ID_1: MsgId = [1, 0, 0, 0, 0, 0, 0, 0xa1];
const ID_2: MsgId = [2, 0, 0, 0, 0, 0, 0, 0xa2];
const ID_3: MsgId = [3, 0, 0, 0, 0, 0, 0, 0xa3];

fn phone_online(w: &MockWorld, g: &Group) -> Phone {
    let mut p = w.phone(&g.dgk, SERVER_GROUP, PHONE_ID);
    p.connect();
    p
}

fn reflected(c: &Client) -> Vec<&Reflected> {
    c.payloads
        .iter()
        .filter_map(|p| match p {
            IncomingPayload::Reflected(r) => Some(r),
            _ => None,
        })
        .collect()
}

/// The order of what the client got, by name.
fn names(c: &Client) -> Vec<&'static str> {
    c.payloads
        .iter()
        .map(|p| match p {
            IncomingPayload::Reflected(r) if r.flags.0 & EPHEMERAL != 0 => "ephemeral",
            IncomingPayload::Reflected(_) => "reflected",
            IncomingPayload::ReflectionQueueDry => "dry",
            IncomingPayload::RolePromotedToLeader => "promoted",
            _ => "other",
        })
        .collect()
}

fn ack(w: &MockWorld, c: &mut Client, reflect_id: u32) {
    send(
        w,
        c,
        OutgoingPayload::ReflectedAck(ReflectedAck { reflect_id }),
    );
}

/// Opens a reflection the way Kourier's follower will (P5) and decodes it.
#[track_caller]
fn open(dgk: [u8; 32], r: &Reflected) -> d2d::Envelope {
    let (_nonce, plaintext) = DeviceGroupKey::from(dgk)
        .open_reflected(&r.envelope)
        .expect("P5 opens what the phone reflected");
    d2d::Envelope::decode(plaintext.as_slice()).expect("the plaintext is a d2d.Envelope")
}

/// What the hook saw of one ack: the id, the length of the acked device's queue then, the virtual time.
type Seen = (u32, Option<usize>, u64);

fn author() -> Author {
    Author::new(PHONE_ID)
}

/// What the phone reflects opens with P5 (libthreema's open of the fake's own seal), the bytes are the
/// bytes the phone sealed, and the plaintext decodes to the built envelope; the header carries the
/// mediator's time and the slot's id; a message of each family goes through.
#[test]
fn a_reflected_envelope_opens_with_p5_and_decodes_to_the_built_envelope() {
    let w = world(501);
    let g = group(&w);
    let mut phone = phone_online(&w, &g);
    let mut c = online(&w, &g, new_device(KOURIER_ID));
    let a = author();
    w.clock().advance(Duration::from_millis(250));
    let built = [
        a.incoming(PEER, &Msg::text(ID_1, START_MS, "hello").nonce([7; 24])),
        a.outgoing(
            &Conv::group("CREACCCC", 0x55),
            &Msg::text(ID_2, START_MS, "to a group")
                .in_group("CREACCCC", 0x55)
                .nonce([8; 24]),
        ),
        a.group_create(GroupDelta::create(
            "CREACCCC",
            0x55,
            "Team",
            &[PEER],
            START_MS,
        )),
        a.sent(&[(Conv::contact(PEER), ID_2)]),
    ];
    let mut sealed = Vec::new();
    for envelope in &built {
        sealed.push(phone.reflect_envelope(envelope));
    }
    pump(w.mediator(), &mut c);
    let got = reflected(&c);
    assert!(
        got.len() == built.len(),
        "the client got {} of {} reflections",
        got.len(),
        built.len()
    );
    for ((r, envelope), reflection) in got.iter().zip(&built).zip(&sealed) {
        assert!(
            r.envelope == reflection.sealed,
            "the bytes differ from the phone's seal"
        );
        assert!(r.timestamp == START_MS + 250, "the mediator's time");
        assert!(r.flags.0 == 0, "a stored message carries no flag");
        assert!(
            &open(g.dgk, r) == envelope,
            "the opened plaintext is not the built envelope"
        );
    }
    let ids: Vec<u32> = got.iter().map(|r| r.reflect_id).collect();
    assert!(
        ids.windows(2).all(|p| p[1] == p[0].wrapping_add(1)),
        "the slot's ids run in order"
    );
    w.violations().assert_empty();
}

/// A typing indicator is reflected EPHEMERAL: the flag is set, nothing is queued, the phone gets no
/// reflect-ack. The same typing reflected WITHOUT the flag (the kind decides the event, the flag the
/// ack) is queued and acked like a message.
#[test]
fn a_typing_indicator_is_reflected_ephemeral_and_one_without_the_flag_is_stored() {
    let w = world(502);
    let g = group(&w);
    let mut phone = phone_online(&w, &g);
    let mut c = online(&w, &g, new_device(KOURIER_ID));
    let typing = author().incoming(PEER, &Msg::typing(ID_1, START_MS, 1));
    assert!(
        flags_for(&typing) == EPHEMERAL,
        "a typing indicator is ephemeral by its kind"
    );
    let first = phone.reflect_envelope(&typing);
    pump(w.mediator(), &mut c);
    assert!(first.flags == EPHEMERAL);
    let got = reflected(&c);
    assert!(
        got.len() == 1 && got[0].flags.0 & EPHEMERAL != 0,
        "the ephemeral flag did not reach the client"
    );
    assert!(open(g.dgk, got[0]) == typing, "the typing envelope");
    assert!(
        w.mediator().queue_len(&g.keys.path_public, KOURIER_ID) == Some(0),
        "an ephemeral reflect was queued"
    );
    assert!(
        !phone
            .take_events()
            .iter()
            .any(|e| matches!(e, PhoneEvent::ReflectAck { .. })),
        "an ephemeral reflect was acked to the phone"
    );
    // The same kind, flag 0: stored, acked.
    let second = phone.reflect_with(&typing, 0);
    pump(w.mediator(), &mut c);
    assert!(second.flags == 0);
    assert!(
        w.mediator().queue_len(&g.keys.path_public, KOURIER_ID) == Some(1),
        "a typing without the flag was not stored"
    );
    assert!(
        phone
            .take_events()
            .iter()
            .any(|e| matches!(e, PhoneEvent::ReflectAck { id, .. } if *id == second.reflect_id)),
        "a typing without the flag was not acked to the phone"
    );
    assert!(reflected(&c).len() == 2 && reflected(&c)[1].flags.0 == 0);
    w.violations().assert_empty();
}

/// A replay is the same sealed bytes under a new id; a flipped byte and a 39-byte box do not open with
/// P5; garbage opens and does not decode; a plaintext with a oneof member of a newer build decodes with
/// `content` unset.
#[test]
fn replays_tampered_short_garbage_and_newer_envelopes() {
    let w = world(503);
    let g = group(&w);
    let mut phone = phone_online(&w, &g);
    let mut c = online(&w, &g, new_device(KOURIER_ID));
    let a = author();
    let first = phone.reflect_envelope(&a.incoming(PEER, &Msg::text(ID_1, START_MS, "x")));
    let replay = phone.reflect_bytes(&first.sealed, 0);
    let bad = phone.reflect_bytes(&tampered(&first.sealed, 30), 0);
    let short = phone.reflect_bytes(&too_short(), 0);
    let garbage_box = phone.seal_bytes(&[0xff; 12]);
    let garbage = phone.reflect_bytes(&garbage_box, 0);
    let newer_box = phone.seal_bytes(&a.unknown_content_plaintext());
    let newer = phone.reflect_bytes(&newer_box, 0);
    pump(w.mediator(), &mut c);
    let got = reflected(&c);
    assert!(got.len() == 6, "six reflections reach the client");
    assert!(
        got[1].envelope == first.sealed && got[1].reflect_id != got[0].reflect_id,
        "a replay is the same bytes"
    );
    let key = DeviceGroupKey::from(g.dgk);
    assert!(
        key.open_reflected(&got[2].envelope).is_err(),
        "a flipped byte opened"
    );
    assert!(
        key.open_reflected(&got[3].envelope).is_err(),
        "39 bytes opened"
    );
    let (_, plaintext) = key
        .open_reflected(&got[4].envelope)
        .expect("garbage still opens");
    assert!(
        d2d::Envelope::decode(plaintext.as_slice()).is_err(),
        "garbage decoded"
    );
    let (_, plaintext) = key
        .open_reflected(&got[5].envelope)
        .expect("a newer envelope opens");
    let decoded = d2d::Envelope::decode(plaintext.as_slice()).expect("a newer envelope decodes");
    assert!(decoded.content.is_none() && decoded.device_id == PHONE_ID);
    assert!(
        [replay, bad, short, garbage, newer]
            .iter()
            .all(|r| r.reflect_id > first.reflect_id)
    );
    w.violations().assert_empty();
}

/// B1: the reflections queued with `reflect_during_join` reach the NEXT new device right after its first
/// `ServerInfo` (which counts the stored ones), in the order added, ephemeral ones after the backlog,
/// all before Dry; they open and decode; they are stored until acked; the list is taken once, so another
/// new device gets none, and an EXISTING device's reconnect gets its un-acked backlog only.
#[test]
fn reflect_during_join_delivers_after_server_info_before_dry_in_order() {
    let w = world(504);
    let g = group(&w);
    let mut phone = w.phone(&g.dgk, SERVER_GROUP, PHONE_ID);
    let a = author();
    let envelopes = [
        a.incoming(PEER, &Msg::text(ID_1, START_MS, "first").nonce([1; 24])),
        a.contact_create(ContactDelta::create(PEER, [9; 32], START_MS)),
        a.incoming(PEER, &Msg::typing(ID_3, START_MS, 1)),
    ];
    for envelope in &envelopes {
        let flags = flags_for(envelope);
        let sealed = phone.seal(envelope);
        w.mediator().reflect_during_join(flags, sealed);
    }
    let c = online(&w, &g, new_device(KOURIER_ID));
    assert!(
        names(&c) == vec!["reflected", "reflected", "ephemeral", "dry", "promoted"],
        "the order after ServerInfo: {:?}",
        names(&c)
    );
    let info = c.post_handshake.as_ref().expect("a ServerInfo");
    assert!(
        info.reflection_queue_length == 2,
        "ServerInfo counts the stored reflections (2), not the ephemeral one"
    );
    let got = reflected(&c);
    assert!(got[1].reflect_id == got[0].reflect_id.wrapping_add(1));
    assert!(got[2].flags.0 == EPHEMERAL && got[2].timestamp == START_MS);
    for (r, envelope) in got.iter().zip(&envelopes) {
        assert!(
            &open(g.dgk, r) == envelope,
            "a join reflection differs from the built envelope"
        );
    }
    let (first_id, second_id) = (got[0].reflect_id, got[1].reflect_id);
    let m = w.mediator();
    assert!(
        m.queue_len(&g.keys.path_public, KOURIER_ID) == Some(2),
        "the stored ones wait for an ack"
    );
    assert!(
        w.trace()
            .lines()
            .iter()
            .any(|l| l.contains("reflect-during-join count=3")),
        "the trace does not say the join reflections were delivered"
    );
    // The list was taken: the next new device gets nothing.
    let other = online(&w, &g, new_device(OTHER_ID));
    assert!(names(&other) == vec!["dry"], "the list is taken once");
    // An existing device that comes back finds its un-acked backlog (the two stored ones), once more.
    m.client_close(c.conn(), 1000);
    let again = online(&w, &g, existing(KOURIER_ID));
    assert!(
        names(&again) == vec!["reflected", "reflected", "dry"],
        "the redelivery: {:?}",
        names(&again)
    );
    assert!(
        reflected(&again)
            .iter()
            .map(|r| r.envelope.clone())
            .collect::<Vec<_>>()
            == got[..2]
                .iter()
                .map(|r| r.envelope.clone())
                .collect::<Vec<_>>()
    );
    // And the knob is a one-shot for the NEXT new slot only: an existing device does not use it.
    let late = phone.seal(&a.incoming(PEER, &Msg::text(ID_2, START_MS, "late")));
    m.reflect_during_join(0, late);
    m.client_close(again.conn(), 1000);
    let mut again = online(&w, &g, existing(KOURIER_ID));
    assert!(
        names(&again) == vec!["reflected", "reflected", "dry"],
        "an existing slot used the join knob"
    );
    ack(&w, &mut again, first_id);
    ack(&w, &mut again, second_id);
    assert!(m.queue_len(&g.keys.path_public, KOURIER_ID) == Some(0));
    let third = online(&w, &g, new_device(THIRD_ID));
    assert!(
        names(&third) == vec!["reflected", "dry"],
        "the late reflection went to the next new device"
    );
    assert!(w.mediator().reflected_acks().len() == 2);
    w.violations().assert_empty();
}

/// L11: every `ReflectedAck` is recorded with the connection, the device, the id, the virtual time it
/// came at and whether the queue held the id; in the order it arrived, not the order of the ids.
#[test]
fn the_ack_list_records_the_ids_in_order_with_the_virtual_time() {
    let w = world(505);
    let g = group(&w);
    let mut phone = phone_online(&w, &g);
    let mut c = online(&w, &g, new_device(KOURIER_ID));
    let a = author();
    let ids: Vec<u32> = [ID_1, ID_2, ID_3]
        .iter()
        .map(|id| {
            phone
                .reflect_envelope(&a.incoming(PEER, &Msg::text(*id, START_MS, "x")))
                .reflect_id
        })
        .collect();
    pump(w.mediator(), &mut c);
    let slot_ids: Vec<u32> = reflected(&c).iter().map(|r| r.reflect_id).collect();
    assert!(slot_ids.len() == 3);
    assert!(
        w.mediator().reflected_acks().is_empty(),
        "an ack before any was sent"
    );
    w.clock().advance(Duration::from_secs(5));
    ack(&w, &mut c, slot_ids[2]);
    w.clock().advance(Duration::from_secs(7));
    ack(&w, &mut c, slot_ids[0]);
    let conn = c.conn();
    let want = vec![
        AckSeen {
            conn,
            device: KOURIER_ID,
            reflected_id: slot_ids[2],
            at_ms: START_MS + 5_000,
            known: true,
        },
        AckSeen {
            conn,
            device: KOURIER_ID,
            reflected_id: slot_ids[0],
            at_ms: START_MS + 12_000,
            known: true,
        },
    ];
    assert!(
        w.mediator().reflected_acks() == want,
        "the recorded acks differ"
    );
    assert!(
        w.mediator().queue_len(&g.keys.path_public, KOURIER_ID) == Some(1),
        "one entry is left"
    );
    assert!(ids.len() == 3);
    w.violations().assert_empty();
}

/// The hook runs once per ack, in order, AFTER the mediator took the ack (the queue it reads is already
/// shorter) and with the mediator's lock released (it may call the mediator without a deadlock), and
/// BEFORE the ack shows in `reflected_acks` (a waiter on that list never finds the hook unfinished).
#[test]
fn the_ack_hook_sees_the_state_after_the_ack_and_may_read_the_mediator() {
    let w = world(506);
    let g = group(&w);
    let mut phone = phone_online(&w, &g);
    let mut c = online(&w, &g, new_device(KOURIER_ID));
    let a = author();
    for id in [ID_1, ID_2] {
        phone.reflect_envelope(&a.incoming(PEER, &Msg::text(id, START_MS, "x")));
    }
    pump(w.mediator(), &mut c);
    let seen: Arc<Mutex<Vec<Seen>>> = Arc::default();
    let (mediator, clock, path) = (w.mediator().clone(), w.clock().clone(), g.keys.path_public);
    let sink = Arc::clone(&seen);
    let published: Arc<Mutex<Vec<bool>>> = Arc::default();
    let in_hook = Arc::clone(&published);
    w.mediator().on_reflected_ack(move |ack| {
        // The hook reads the mediator: this would hang if its lock were held.
        let left = mediator.queue_len(&path, ack.device);
        // While its hook runs, an ack is not in `reflected_acks` yet: a test that waits for the ack
        // there and then reads what the hook recorded cannot find the hook unfinished.
        let shown = mediator
            .reflected_acks()
            .iter()
            .any(|a| a.reflected_id == ack.reflected_id);
        in_hook.lock().expect("the hook's flags").push(shown);
        sink.lock()
            .expect("the hook's list")
            .push((ack.reflected_id, left, clock.now_ms()));
    });
    let slot_ids: Vec<u32> = reflected(&c).iter().map(|r| r.reflect_id).collect();
    w.clock().advance(Duration::from_secs(1));
    ack(&w, &mut c, slot_ids[0]);
    ack(&w, &mut c, slot_ids[1]);
    let seen = seen.lock().expect("the hook's list").clone();
    assert!(
        seen == vec![
            (slot_ids[0], Some(1), START_MS + 1_000),
            (slot_ids[1], Some(0), START_MS + 1_000)
        ],
        "the hook's calls differ"
    );
    assert!(
        w.mediator().reflected_acks().len() == 2,
        "the list is kept beside the hook"
    );
    assert!(
        *published.lock().expect("the hook's flags") == vec![false, false],
        "an ack was in reflected_acks while its hook still ran"
    );
    w.violations().assert_empty();
}

/// An ack of an id the slot does not hold (an ephemeral one, say) is recorded as unknown, is a
/// violation and closes the connection with 4012.
#[test]
fn an_ack_of_an_ephemeral_reflection_is_recorded_unknown_and_is_a_violation() {
    let w = world(507);
    let g = group(&w);
    let mut phone = phone_online(&w, &g);
    let mut c = online(&w, &g, new_device(KOURIER_ID));
    phone.reflect_envelope(&author().incoming(PEER, &Msg::typing(ID_1, START_MS, 1)));
    pump(w.mediator(), &mut c);
    let id = reflected(&c)[0].reflect_id;
    ack(&w, &mut c, id);
    let acks = w.mediator().reflected_acks();
    assert!(
        acks.len() == 1 && !acks[0].known && acks[0].reflected_id == id,
        "the unknown ack"
    );
    assert!(
        c.closed == Some(4012),
        "the connection was not closed with 4012"
    );
    assert_eq!(
        w.violations().rules(),
        vec!["reflected-ack-for-an-unknown-id"]
    );
}

/// The id of the message is the wire bytes as a little-endian `fixed64` all the way through the fake:
/// what the client opens names the same message the builder was given.
#[test]
fn the_message_id_survives_the_wire_as_the_given_bytes() {
    let w = world(508);
    let g = group(&w);
    let mut phone = phone_online(&w, &g);
    let mut c = online(&w, &g, new_device(KOURIER_ID));
    let id: MsgId = [0xf1, 0xe2, 0xd3, 0xc4, 0xb5, 0xa6, 0x97, 0x88];
    phone.reflect_envelope(&author().incoming(PEER, &Msg::text(id, START_MS, "x")));
    pump(w.mediator(), &mut c);
    let Some(d2d::envelope::Content::IncomingMessage(m)) = open(g.dgk, reflected(&c)[0]).content
    else {
        panic!("not an incoming message");
    };
    assert!(m.message_id == id_u64(id) && m.message_id.to_le_bytes() == id);
    w.violations().assert_empty();
}
