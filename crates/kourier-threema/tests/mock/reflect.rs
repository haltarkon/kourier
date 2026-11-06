//! The fake mediator, part 2 (`support/mock/d2m.rs` reflection, `support/mock/phone.rs`), against
//! the REAL patched `D2mProtocol` and the fake's own phone: proofs 4, 5, 6 and 7 of lane plan §5.2,
//! the queue limit (4114, D18) and both sides of the encrypted envelope's limit (L3). One thread,
//! no runtime, no unshare (as `d2m.rs`).

use kourier_threema::close::{LoggedOut, Verdict, verdict};
use libthreema::d2m::payload::{
    IncomingPayload, OutgoingPayload, Reflect, ReflectFlags, Reflected, ReflectedAck,
};
use libthreema::protobuf::d2d;

use super::d2m::{
    Client, Group, Opts, SERVER_GROUP, client, connect, existing, group, new_device, online, pump,
    send,
};
use crate::support::mock::d2m::{
    EPHEMERAL, MAX_DATAGRAM, MAX_ENVELOPE, REFLECT_HEADER, ReflectSeen, VOLATILE_KEEP, datagram,
    kind, parse_reflect, reflect_ack_payload, reflected_payload,
};
use crate::support::mock::phone::{Phone, PhoneEvent, incoming_text, open_envelope};
use crate::support::mock::{MockWorld, START_MS, world};

const PHONE_ID: u64 = 0x0FF0_0000_0000_0001;
const KOURIER_ID: u64 = 0xF000_0000_0000_0002;
const OTHER_ID: u64 = 0x0000_0000_0000_0003;

fn phone_online(w: &MockWorld, g: &Group) -> Phone {
    let mut p = w.phone(&g.dgk, SERVER_GROUP, PHONE_ID);
    p.connect();
    p
}

/// A device that logged in once and went offline (its PERSISTENT slot stays).
fn offline_slot(w: &MockWorld, g: &Group, device: u64) {
    let c = online(w, g, new_device(device));
    w.mediator().client_close(c.conn(), 1000);
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

fn reflect_acks(c: &Client) -> Vec<(u32, u64)> {
    c.payloads
        .iter()
        .filter_map(|p| match p {
            IncomingPayload::ReflectAck(a) => Some((a.reflect_id, a.timestamp)),
            _ => None,
        })
        .collect()
}

/// The order of what the client got, by name (for the Dry / promotion / backlog checks).
fn names(c: &Client) -> Vec<&'static str> {
    c.payloads
        .iter()
        .map(|p| match p {
            IncomingPayload::Reflected(_) => "reflected",
            IncomingPayload::ReflectAck(_) => "reflect-ack",
            IncomingPayload::ReflectionQueueDry => "dry",
            IncomingPayload::RolePromotedToLeader => "promoted",
            IncomingPayload::Proxy(_) => "proxy",
            _ => "other",
        })
        .collect()
}

fn reflect(w: &MockWorld, c: &mut Client, reflect_id: u32, flags: u16, envelope: Vec<u8>) {
    send(
        w,
        c,
        OutgoingPayload::Reflect(Reflect {
            flags: ReflectFlags(flags),
            reflect_id,
            envelope,
        }),
    );
}

fn ack(w: &MockWorld, c: &mut Client, reflect_id: u32) {
    send(
        w,
        c,
        OutgoingPayload::ReflectedAck(ReflectedAck { reflect_id }),
    );
}

fn text_envelope(p: &Phone, message_id: u64, text: &str) -> d2d::Envelope {
    let nonce = [9u8; 24];
    incoming_text(
        p.device(),
        "PEER0001",
        message_id,
        START_MS + 123,
        text,
        nonce,
    )
}

// --- Proof 4: reflect, ack, fan-out ----------------------------------------------------------

/// The sender gets a `ReflectAck` with the reflect-id and the time the entry was stored; every
/// OTHER slot (an offline one too) gets the envelope byte for byte with an id of its own sequence;
/// the sender gets nothing back; an ack empties the entry.
#[test]
fn a_reflect_is_acked_with_the_time_and_queued_for_every_other_slot_with_its_own_id() {
    let w = world(401);
    let g = group(&w);
    let mut phone = phone_online(&w, &g);
    offline_slot(&w, &g, OTHER_ID);
    let mut c = online(&w, &g, new_device(KOURIER_ID));
    w.clock().advance(std::time::Duration::from_millis(1234));
    let sealed = phone.seal(&text_envelope(&phone, 1, "from kourier"));
    reflect(&w, &mut c, 7, 0, sealed.clone());
    assert_eq!(
        reflect_acks(&c),
        vec![(7, START_MS + 1234)],
        "the reflect-ack"
    );
    assert!(
        reflected(&c).is_empty(),
        "the sender got its own reflect back"
    );
    let at_phone = phone.take_reflected();
    assert_eq!(at_phone.len(), 1, "the phone's share");
    assert!(
        at_phone[0].1 == sealed,
        "the phone's envelope is not the one reflected"
    );
    assert!(
        phone.take_events().contains(&PhoneEvent::Reflected {
            id: at_phone[0].0,
            flags: 0,
            timestamp: START_MS + 1234
        }),
        "the phone's Reflected header"
    );
    let m = w.mediator();
    assert_eq!(
        m.queue_len(&g.keys.path_public, PHONE_ID),
        Some(1),
        "the phone's queue"
    );
    assert_eq!(
        m.queue_len(&g.keys.path_public, OTHER_ID),
        Some(1),
        "the offline slot's queue"
    );
    assert_eq!(
        m.queue_len(&g.keys.path_public, KOURIER_ID),
        Some(0),
        "the sender's queue"
    );
    let other = online(&w, &g, existing(OTHER_ID));
    let info = other.post_handshake.as_ref().expect("a ServerInfo");
    assert_eq!(
        info.reflection_queue_length, 1,
        "the offline slot's backlog"
    );
    let got = reflected(&other);
    assert_eq!(got.len(), 1, "the offline slot's entry");
    assert!(
        got[0].envelope == sealed,
        "the offline slot's envelope differs"
    );
    assert_ne!(
        got[0].reflect_id, at_phone[0].0,
        "two slots share one id sequence"
    );
    phone.ack(at_phone[0].0);
    assert_eq!(
        m.queue_len(&g.keys.path_public, PHONE_ID),
        Some(0),
        "the ack did not empty it"
    );
    w.violations().assert_empty();
}

/// An ephemeral reflect reaches only the devices online now, is never queued (an offline slot
/// does not get it later) and is not acked; a normal reflect right after it is.
#[test]
fn an_ephemeral_reflect_reaches_only_connected_devices_and_is_not_acked() {
    let w = world(402);
    let g = group(&w);
    let mut phone = phone_online(&w, &g);
    offline_slot(&w, &g, OTHER_ID);
    let mut c = online(&w, &g, new_device(KOURIER_ID));
    let typing = phone.seal(&text_envelope(&phone, 2, "typing"));
    reflect(&w, &mut c, 1, EPHEMERAL, typing.clone());
    assert!(
        reflect_acks(&c).is_empty(),
        "an ephemeral reflect was acked"
    );
    let events = phone.take_events();
    assert!(
        events.iter().any(|e| matches!(
            e,
            PhoneEvent::Reflected {
                flags: EPHEMERAL,
                ..
            }
        )),
        "the online phone did not get the ephemeral reflect"
    );
    assert!(
        phone.take_reflected().iter().any(|(_, e)| *e == typing),
        "its envelope"
    );
    let m = w.mediator();
    assert_eq!(
        m.queue_len(&g.keys.path_public, PHONE_ID),
        Some(0),
        "queued for the phone"
    );
    assert_eq!(
        m.queue_len(&g.keys.path_public, OTHER_ID),
        Some(0),
        "queued offline"
    );
    let other = online(&w, &g, existing(OTHER_ID));
    assert!(
        reflected(&other).is_empty(),
        "the offline slot got the ephemeral reflect later"
    );
    reflect(&w, &mut c, 2, 0, typing);
    assert_eq!(
        reflect_acks(&c).len(),
        1,
        "the normal reflect after it was not acked"
    );
    w.violations().assert_empty();
}

/// libthreema's encoders write `reflect` and `reflected-ack` as T0 pinned them
/// (`T0/src/t0/d2m_mock.rs:210-241`), and the fake writes `reflect-ack` and `reflected` in the
/// spec's layout, with literal bytes.
#[test]
fn reflect_and_reflected_ack_bytes_equal_t0s_goldens() {
    let w = world(403);
    let g = group(&w);
    let mut c = online(&w, &g, new_device(KOURIER_ID));
    let ack = c
        .proto
        .create_payload(OutgoingPayload::ReflectedAck(ReflectedAck {
            reflect_id: 0x0141,
        }))
        .expect("encode")
        .outgoing_datagram
        .expect("a datagram")
        .0;
    assert_eq!(
        ack,
        vec![0x83, 0, 0, 0, 0, 0, 0, 0, 0x41, 0x01, 0, 0],
        "reflected-ack"
    );
    let envelope = vec![0xAB; 40];
    let d = c
        .proto
        .create_payload(OutgoingPayload::Reflect(Reflect {
            flags: ReflectFlags(0),
            reflect_id: 1,
            envelope: envelope.clone(),
        }))
        .expect("encode")
        .outgoing_datagram
        .expect("a datagram")
        .0;
    assert_eq!(
        d[..12],
        [0x80, 0, 0, 0, 8, 0, 0, 0, 1, 0, 0, 0],
        "the reflect's header"
    );
    assert!(d[12..] == envelope[..], "the reflect's envelope");
    assert_eq!(
        reflect_ack_payload(1, 1_760_000_000_500),
        vec![0, 0, 0, 0, 1, 0, 0, 0, 244, 193, 44, 200, 153, 1, 0, 0],
        "the fake's reflect-ack"
    );
    let r = reflected_payload(0, 321, 1_760_000_000_500, &[7, 7]);
    assert_eq!(
        r,
        vec![
            16, 0, 0, 0, 0x41, 0x01, 0, 0, 244, 193, 44, 200, 153, 1, 0, 0, 7, 7
        ],
        "the fake's reflected (T0's header)"
    );
    // The fake's decoder takes what libthreema encoded.
    w.mediator().receive(c.conn(), d);
    pump(w.mediator(), &mut c);
    assert_eq!(
        reflect_acks(&c).iter().map(|a| a.0).collect::<Vec<_>>(),
        vec![1],
        "acked"
    );
    w.violations().assert_empty();
}

/// A reflect-id is unique per connection: a repeat closes with 4010 (recorded); a new connection
/// may use it again.
#[test]
fn a_repeated_reflect_id_on_one_connection_is_closed_with_4010() {
    let w = world(404);
    let g = group(&w);
    let mut c = online(&w, &g, new_device(KOURIER_ID));
    reflect(&w, &mut c, 5, 0, vec![1; 40]);
    reflect(&w, &mut c, 5, 0, vec![2; 40]);
    assert_eq!(c.closed, Some(4010), "a repeated reflect-id");
    let mut again = online(&w, &g, existing(KOURIER_ID));
    reflect(&w, &mut again, 5, 0, vec![3; 40]);
    assert_eq!(reflect_acks(&again).len(), 1, "the id on a new connection");
    assert_eq!(again.closed, None, "the new connection was closed");
    // The check is the last statement (G12): the new connection broke nothing either.
    assert_eq!(w.violations().rules(), vec!["reflect-id-repeated"]);
}

/// G1 (the spec review's second MEDIUM-1 experiment): a VOLATILE device that took its connection
/// over (4112) still gets what another device reflects after the keep time, and its
/// `ReflectedAck` finds its slot (the fake panicked there before the fix).
#[test]
fn a_volatile_device_after_a_4112_takeover_still_gets_reflects_and_acks_them() {
    use libthreema::protobuf::d2m::DeviceSlotExpirationPolicy as Expiration;
    use libthreema::protobuf::d2m::DeviceSlotState as SlotState;
    let w = world(405);
    let g = group(&w);
    w.mediator()
        .knobs(|k| k.idle_timeout = std::time::Duration::from_secs(3600));
    let volatile = Opts {
        expiration: Expiration::Volatile,
        ..new_device(OTHER_ID)
    };
    let mut older = online(&w, &g, volatile);
    let mut newer = online(
        &w,
        &g,
        Opts {
            expected: SlotState::Existing,
            ..volatile
        },
    );
    pump(w.mediator(), &mut older);
    assert_eq!(older.closed, Some(4112), "the older connection");
    let mut sender = online(&w, &g, new_device(KOURIER_ID));
    w.mediator()
        .advance(VOLATILE_KEEP + std::time::Duration::from_millis(1));
    reflect(&w, &mut sender, 1, 0, vec![4; 40]);
    pump(w.mediator(), &mut newer);
    let got = reflected(&newer);
    assert_eq!(
        got.len(),
        1,
        "the reflect did not reach the volatile device"
    );
    let id = got[0].reflect_id;
    ack(&w, &mut newer, id);
    assert_eq!(
        w.mediator().queue_len(&g.keys.path_public, OTHER_ID),
        Some(0),
        "the ack found no entry"
    );
    assert!(newer.online(), "the volatile device was closed");
    w.violations().assert_empty();
}

// --- Proof 5: the Reflected header after P1 ---------------------------------------------------

/// The fake's 16-byte `Reflected` header: libthreema (with patch P1) hands over the WHOLE
/// envelope, byte for byte, and the oracle opens it with DGRK to the phone's message. Without P1
/// the envelope loses its first byte and does not open (the M3 mutant).
#[test]
fn a_reflected_envelope_decodes_whole_after_p1_and_opens_with_dgrk() {
    let w = world(501);
    let g = group(&w);
    let mut phone = phone_online(&w, &g);
    let mut c = online(&w, &g, new_device(KOURIER_ID));
    let envelope = text_envelope(&phone, 5, "hi from the phone's queue");
    let sealed = phone.reflect(&envelope);
    pump(w.mediator(), &mut c);
    let got = reflected(&c);
    assert_eq!(got.len(), 1, "the client's Reflected");
    assert!(
        got[0].envelope.len() == sealed.len(),
        "the envelope's length differs (P1)"
    );
    assert!(got[0].envelope == sealed, "the envelope differs");
    assert_eq!(
        (got[0].flags.0, got[0].timestamp),
        (0, START_MS),
        "the header"
    );
    let opened = open_envelope(&g.keys, &got[0].envelope).expect("the envelope opens with DGRK");
    assert!(opened == envelope, "the opened envelope is not the phone's");
    let Some(d2d::envelope::Content::IncomingMessage(msg)) = opened.content else {
        panic!("not an IncomingMessage");
    };
    assert!(
        msg.sender_identity == "PEER0001" && msg.message_id == 5,
        "the message's sender or id"
    );
    assert!(
        msg.body == b"hi from the phone's queue",
        "the message's body"
    );
    w.violations().assert_empty();
}

// --- Proof 6: redelivery and 4012 -------------------------------------------------------------

/// What was not acked comes again after a reconnect, with the same id, before Dry; what was acked
/// does not.
#[test]
fn a_reconnect_without_the_ack_gets_the_entry_again_and_an_acked_one_does_not() {
    let w = world(601);
    let g = group(&w);
    let mut phone = phone_online(&w, &g);
    let mut c = online(&w, &g, new_device(KOURIER_ID));
    let first = phone.reflect(&text_envelope(&phone, 1, "one"));
    let second = phone.reflect(&text_envelope(&phone, 2, "two"));
    pump(w.mediator(), &mut c);
    let ids: Vec<u32> = reflected(&c).iter().map(|r| r.reflect_id).collect();
    assert_eq!(ids.len(), 2, "both entries");
    assert!(reflected(&c)[0].envelope == first, "the queue's order");
    ack(&w, &mut c, ids[0]);
    w.mediator().client_close(c.conn(), 1000);
    let again = online(&w, &g, existing(KOURIER_ID));
    assert_eq!(
        again
            .post_handshake
            .as_ref()
            .expect("a ServerInfo")
            .reflection_queue_length,
        1,
        "the backlog"
    );
    let got = reflected(&again);
    assert_eq!(got.len(), 1, "only the entry that was not acked");
    assert!(got[0].envelope == second, "the redelivered envelope");
    assert_eq!(got[0].reflect_id, ids[1], "the redelivered id");
    assert_eq!(
        names(&again),
        vec!["reflected", "dry"],
        "the backlog before Dry"
    );
    w.violations().assert_empty();
}

/// An ack for an id the slot does not hold (never sent, or acked already) closes with 4012 and is
/// recorded.
#[test]
fn an_ack_for_an_id_the_slot_does_not_hold_is_closed_with_4012() {
    let w = world(602);
    let g = group(&w);
    let mut phone = phone_online(&w, &g);
    let mut c = online(&w, &g, new_device(KOURIER_ID));
    phone.reflect(&text_envelope(&phone, 1, "one"));
    pump(w.mediator(), &mut c);
    let id = reflected(&c)[0].reflect_id;
    ack(&w, &mut c, id);
    assert_eq!(c.closed, None, "a good ack was refused");
    ack(&w, &mut c, id);
    assert_eq!(c.closed, Some(4012), "an ack of an id acked already");
    let mut d = online(&w, &g, existing(KOURIER_ID));
    ack(&w, &mut d, id.wrapping_add(1000));
    assert_eq!(d.closed, Some(4012), "an ack of an id never sent");
    assert_eq!(
        w.violations().rules(),
        vec![
            "reflected-ack-for-an-unknown-id",
            "reflected-ack-for-an-unknown-id"
        ]
    );
}

// --- Proof 7: Dry, the promotion, the leader --------------------------------------------------

/// The first device of the group gets Dry and then the promotion; a later one gets the backlog,
/// Dry and no promotion; when the leader goes, the other is promoted; one leader at a time; the
/// unblock hook says yes only to the leader of a test in leader mode.
#[test]
fn dry_comes_before_the_promotion_and_the_leader_moves_when_the_leader_goes() {
    let w = world(701);
    // The unblock assertions below are about the knob turned OFF (its default is on since TH6 L4b).
    w.mediator().knobs(|k| k.leader_mode = false);
    let g = group(&w);
    let mut phone = phone_online(&w, &g);
    assert_eq!(
        phone.take_events(),
        vec![
            PhoneEvent::ServerInfo { queue_length: 0 },
            PhoneEvent::Dry,
            PhoneEvent::Promoted
        ],
        "the phone's login"
    );
    let m = w.mediator();
    let phone_conn = phone.conn().expect("connected");
    assert_eq!(
        m.leader(&g.keys.path_public),
        Some(phone_conn),
        "the leader"
    );
    let mut c = online(&w, &g, new_device(KOURIER_ID));
    assert_eq!(names(&c), vec!["dry"], "a follower's login");
    assert!(!m.is_leader(c.conn()), "two leaders");
    assert!(
        !m.unblock_allowed(phone_conn),
        "unblock outside leader mode"
    );
    phone.disconnect();
    pump(m, &mut c);
    assert_eq!(
        names(&c),
        vec!["dry", "promoted"],
        "the promotion when the leader went"
    );
    assert_eq!(
        m.leader(&g.keys.path_public),
        Some(c.conn()),
        "the new leader"
    );
    phone.connect();
    assert!(
        !phone.take_events().contains(&PhoneEvent::Promoted),
        "the phone came back as a second leader"
    );
    assert!(!m.unblock_allowed(c.conn()), "unblock outside leader mode");
    m.knobs(|k| k.leader_mode = true);
    assert!(m.unblock_allowed(c.conn()), "the leader in leader mode");
    assert!(
        !m.unblock_allowed(phone.conn().expect("connected")),
        "a follower in leader mode"
    );
    w.violations().assert_empty();
}

/// L4: the order of Dry and the promotion is a knob ([U: real server]); with it on, the
/// promotion comes first. With the automatic leader off, only `promote` makes one.
#[test]
fn the_promote_before_dry_knob_turns_the_order_and_promote_makes_a_leader() {
    let w = world(702);
    let g = group(&w);
    w.mediator().knobs(|k| k.promote_before_dry = true);
    let c = online(&w, &g, new_device(KOURIER_ID));
    assert_eq!(names(&c), vec!["promoted", "dry"], "the knob's order");
    let w = world(703);
    let g = group(&w);
    w.mediator().knobs(|k| k.auto_leader = false);
    let mut c = online(&w, &g, new_device(KOURIER_ID));
    assert_eq!(
        names(&c),
        vec!["dry"],
        "a leader without the automatic rule"
    );
    assert_eq!(w.mediator().leader(&g.keys.path_public), None, "a leader");
    w.mediator().promote(c.conn());
    pump(w.mediator(), &mut c);
    assert_eq!(names(&c), vec!["dry", "promoted"], "the scripted promotion");
    w.violations().assert_empty();
}

/// The backlog comes in queue order before Dry; what is reflected later comes after it; no second
/// Dry.
#[test]
fn a_backlog_comes_before_dry_and_what_comes_later_after_it() {
    let w = world(704);
    let g = group(&w);
    let mut phone = phone_online(&w, &g);
    offline_slot(&w, &g, KOURIER_ID);
    let sent: Vec<Vec<u8>> = (1..=3)
        .map(|i| phone.reflect(&text_envelope(&phone, i, "backlog")))
        .collect();
    let mut c = online(&w, &g, existing(KOURIER_ID));
    assert_eq!(
        c.post_handshake
            .as_ref()
            .expect("a ServerInfo")
            .reflection_queue_length,
        3,
        "the backlog's length"
    );
    let later = phone.reflect(&text_envelope(&phone, 4, "later"));
    pump(w.mediator(), &mut c);
    assert_eq!(
        names(&c),
        vec!["reflected", "reflected", "reflected", "dry", "reflected"],
        "the order"
    );
    let envelopes: Vec<&Vec<u8>> = reflected(&c).iter().map(|r| &r.envelope).collect();
    assert!(
        envelopes[..3] == sent.iter().collect::<Vec<_>>()[..],
        "the backlog's order"
    );
    assert!(*envelopes[3] == later, "the later entry");
    let ids: Vec<u32> = reflected(&c).iter().map(|r| r.reflect_id).collect();
    assert!(
        ids.windows(2).all(|p| p[1] == p[0].wrapping_add(1)),
        "the slot's ids are not one increasing sequence"
    );
    w.violations().assert_empty();
}

// --- D18: the queue limit, 4114 ---------------------------------------------------------------

/// One entry over the queue limit: an online device's connection closes with 4114 (logged out:
/// `src/close.rs`), the slot is gone (its next login as EXISTING gets 4115); an offline slot goes
/// the same way.
#[test]
fn a_queue_over_its_limit_closes_the_slot_with_4114_and_deletes_it() {
    let w = world(1801);
    let g = group(&w);
    w.mediator().knobs(|k| k.queue_limit = 2);
    let mut phone = phone_online(&w, &g);
    let mut c = online(&w, &g, new_device(KOURIER_ID));
    offline_slot(&w, &g, OTHER_ID);
    for i in 1..=2 {
        phone.reflect(&text_envelope(&phone, i, "fill"));
    }
    pump(w.mediator(), &mut c);
    assert_eq!(c.closed, None, "closed at the limit");
    phone.reflect(&text_envelope(&phone, 3, "over"));
    pump(w.mediator(), &mut c);
    assert_eq!(c.closed, Some(4114), "the close code over the limit");
    assert_eq!(
        verdict(4114, None),
        Verdict::LoggedOut(LoggedOut::QueueOverflow),
        "close.rs's verdict"
    );
    let slots = w.mediator().slots(&g.keys.path_public);
    assert_eq!(slots, vec![PHONE_ID], "the slots left");
    let mut back = client(g.dgk, existing(KOURIER_ID));
    connect(&w, &mut back);
    assert_eq!(back.closed, Some(4115), "the deleted slot's next login");
    let acked = phone
        .take_events()
        .iter()
        .filter(|e| matches!(e, PhoneEvent::ReflectAck { .. }))
        .count();
    assert_eq!(acked, 3, "the phone's reflects were acked");
    w.violations().assert_empty();
}

// --- L3: the encrypted envelope's limit, both sides --------------------------------------------

/// A `Reflect` whose envelope is 65,516 bytes (what a 16-byte `Reflected` header leaves of
/// 65,536) is taken and reaches the phone in a `Reflected` of exactly 65,536 bytes; one byte more
/// is refused with 4010 (recorded; [U: real server] the code).
#[test]
fn a_reflected_envelope_of_65516_bytes_is_taken_and_one_of_65517_refused() {
    assert_eq!(MAX_ENVELOPE, 65_516, "the spec's figure");
    let w = world(1401);
    let g = group(&w);
    let mut phone = phone_online(&w, &g);
    let c = online(&w, &g, new_device(KOURIER_ID));
    let raw = |id: u32, n: usize| {
        let mut p = vec![u8::try_from(REFLECT_HEADER).expect("8"), 0, 0, 0];
        p.extend_from_slice(&id.to_le_bytes());
        p.extend(std::iter::repeat_n(0x5A, n));
        datagram(kind::REFLECT, &p)
    };
    w.mediator().receive(c.conn(), raw(1, MAX_ENVELOPE));
    let at_phone = phone.take_reflected();
    assert_eq!(at_phone.len(), 1, "the largest envelope did not arrive");
    assert_eq!(
        at_phone[0].1.len() + 4 + 16,
        MAX_DATAGRAM,
        "the Reflected's size"
    );
    assert_eq!(
        w.mediator().closed(c.conn()),
        None,
        "the largest envelope was refused"
    );
    w.mediator().receive(c.conn(), raw(2, MAX_ENVELOPE + 1));
    assert_eq!(w.mediator().closed(c.conn()), Some(4010), "one byte more");
    assert!(
        phone.take_reflected().is_empty(),
        "the oversized envelope was forwarded"
    );
    assert_eq!(
        w.violations().rules(),
        vec!["reflect-envelope-over-65516-bytes"]
    );
}

// --- TH6a S3: the reflection knobs and the record of the reflects ------------------------------

/// `parse_reflect` (the fake's own decoder) reads `TP/md-d2m.struct.yml` `reflect`: header-length,
/// reserved, flags (u16-le), reflect-id (u32-le), then the envelope AFTER header-length bytes, so a
/// longer header (a field a newer client adds) is skipped; a header under 8 bytes or longer than the
/// payload is no reflect. `reflect_ack_payload` is the spec's `reflect-ack`: 4 reserved bytes, the id,
/// the time.
#[test]
fn parse_reflect_and_the_reflect_ack_have_the_specs_bytes() {
    assert_eq!(
        parse_reflect(&[8, 0, 1, 0, 1, 2, 3, 4, 0xaa, 0xbb]),
        Some((1, 0x0403_0201, &[0xaa, 0xbb][..])),
        "the plain reflect"
    );
    assert_eq!(
        parse_reflect(&[8, 0, 0, 0, 9, 0, 0, 0]),
        Some((0, 9, &[][..])),
        "an empty envelope"
    );
    assert_eq!(
        parse_reflect(&[12, 0, 0, 0, 9, 0, 0, 0, 7, 7, 7, 7, 0xcc]),
        Some((0, 9, &[0xcc][..])),
        "a header of 12 bytes: the extra field is skipped"
    );
    assert_eq!(
        parse_reflect(&[7, 0, 0, 0, 9, 0, 0, 0, 1]),
        None,
        "a header under 8"
    );
    assert_eq!(
        parse_reflect(&[8, 0, 0, 0, 9, 0, 0]),
        None,
        "a payload under its header"
    );
    assert_eq!(parse_reflect(&[]), None, "nothing");
    assert_eq!(
        reflect_ack_payload(0x0403_0201, 0x1817_1615_1413_1211),
        vec![
            0, 0, 0, 0, 0x01, 0x02, 0x03, 0x04, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18
        ],
        "reserved, the id, the time"
    );
    let w = world(520);
    w.violations().assert_empty();
}

/// `Knobs::withhold_reflect_acks`: the reflect reaches the other device at once and is recorded, the
/// sender's ack is held until `release_reflect_acks`, which sends it with the time the reflect was
/// STORED; the knob stays on (the next one is held too) until it is set back.
#[test]
fn a_withheld_reflect_ack_is_held_until_released() {
    let w = world(521);
    let g = group(&w);
    let mut a = w.phone(&g.dgk, SERVER_GROUP, KOURIER_ID);
    a.connect();
    let mut b = w.phone(&g.dgk, SERVER_GROUP, OTHER_ID);
    b.connect();
    a.take_events();
    b.take_events();
    let conn = a.conn().expect("connected");
    w.mediator().knobs(|k| k.withhold_reflect_acks = true);

    let envelope = incoming_text(KOURIER_ID, "PEER0001", 1, 1, "x", [3; 24]);
    let sealed = a.seal(&envelope);
    assert_eq!(a.reflect_sealed(&sealed, 0), 1);
    assert_eq!(
        a.take_events(),
        Vec::<PhoneEvent>::new(),
        "the ack was not held"
    );
    assert_eq!(
        b.take_reflected().len(),
        1,
        "the other device got it at once"
    );
    assert_eq!(w.mediator().reflect_acks_held(conn), 1);

    // The time moves; the ack names the time the reflect was stored.
    w.advance(std::time::Duration::from_secs(5));
    assert_eq!(w.mediator().release_reflect_acks(conn), 1);
    assert_eq!(
        a.take_events(),
        vec![PhoneEvent::ReflectAck {
            id: 1,
            timestamp: START_MS
        }]
    );
    assert_eq!(w.mediator().release_reflect_acks(conn), 0, "nothing twice");

    // Still on: the next reflect is held too, and two are released in order.
    let second = a.seal(&envelope);
    let third = a.seal(&envelope);
    assert_eq!(a.reflect_sealed(&second, 0), 2);
    assert_eq!(a.reflect_sealed(&third, 0), 3);
    assert_eq!(w.mediator().reflect_acks_held(conn), 2);
    assert_eq!(w.mediator().release_reflect_acks(conn), 2);
    assert_eq!(
        a.take_events(),
        vec![
            PhoneEvent::ReflectAck {
                id: 2,
                timestamp: START_MS + 5_000
            },
            PhoneEvent::ReflectAck {
                id: 3,
                timestamp: START_MS + 5_000
            }
        ]
    );
    // Off: acked at once again.
    w.mediator().knobs(|k| k.withhold_reflect_acks = false);
    let fourth = a.seal(&envelope);
    assert_eq!(a.reflect_sealed(&fourth, 0), 4);
    assert_eq!(
        a.take_events(),
        vec![PhoneEvent::ReflectAck {
            id: 4,
            timestamp: START_MS + 5_000
        }]
    );
    assert_eq!(w.mediator().reflect_acks_held(conn), 0);
    w.violations().assert_empty();
}

/// An ephemeral reflect is never acked, so nothing of it is held; `Mediator::reflects` lists every
/// reflect in order with the connection, the client's reflect-id, the flags and the sealed bytes as
/// sent (a test compares a repeat with the first).
#[test]
fn the_mediator_lists_its_reflects_in_order_ephemeral_ones_too() {
    let w = world(522);
    let g = group(&w);
    let mut a = w.phone(&g.dgk, SERVER_GROUP, KOURIER_ID);
    a.connect();
    let mut b = w.phone(&g.dgk, SERVER_GROUP, OTHER_ID);
    b.connect();
    let conn = a.conn().expect("connected");
    assert!(w.mediator().reflects().is_empty(), "before any reflect");
    w.mediator().knobs(|k| k.withhold_reflect_acks = true);

    let envelope = incoming_text(KOURIER_ID, "PEER0001", 1, 1, "x", [3; 24]);
    let first = a.seal(&envelope);
    let ephemeral = a.seal(&envelope);
    a.reflect_sealed(&first, 0);
    a.reflect_sealed(&ephemeral, EPHEMERAL);
    a.reflect_sealed(&first, 0);
    assert_eq!(
        w.mediator().reflects(),
        vec![
            ReflectSeen {
                conn,
                reflect_id: 1,
                flags: 0,
                bytes: first.clone()
            },
            ReflectSeen {
                conn,
                reflect_id: 2,
                flags: EPHEMERAL,
                bytes: ephemeral
            },
            ReflectSeen {
                conn,
                reflect_id: 3,
                flags: 0,
                bytes: first
            },
        ]
    );
    assert_eq!(
        w.mediator().reflect_acks_held(conn),
        2,
        "the ephemeral one is not owed an ack"
    );
    w.violations().assert_empty();
}

/// RT1 attempt 2, the fake's phone reads the linked-devices list as Threema for Android 6.6.0 does (`phone_row`,
/// `Phone::linked_devices`): a device info that opens under DGDIK and names a platform shows its label and
/// `<version> · <details>`; one that states UNSPECIFIED, a platform number the app lacks, a box under another key or
/// bytes that are no box show "Device" and "Information could not be obtained" whatever the label; an empty label
/// stays empty; the asker is not listed; a device that left is no longer active. `set_device_info` replaces a slot's
/// stored info and finds no slot that is not there.
#[test]
fn the_phones_list_reads_a_device_info_as_threema_for_android_does() {
    use crate::support::mock::crypto::seal_nonce_ahead;
    use crate::support::mock::phone::{INVALID_DETAILS, INVALID_TITLE, PHONE_LABEL};
    use d2d::device_info::Platform;
    use prost::Message as _;

    let w = world(533);
    let g = group(&w);
    let mut phone = phone_online(&w, &g);
    let mut other = w.phone(&g.dgk, SERVER_GROUP, OTHER_ID);
    other.connect();
    let rows = phone.linked_devices();
    assert_eq!(rows.len(), 1, "the asker is listed or the other is not");
    let row = &rows[0];
    assert!(
        row.device == OTHER_ID
            && row.platform == Platform::Android
            && row.valid
            && row.active
            && row.title == PHONE_LABEL
            && row.details == "0 · fake",
        "the other phone's own info does not read as its label, version and details"
    );

    let info = |platform: i32, label: &str| {
        d2d::DeviceInfo {
            #[expect(deprecated, reason = "the spec's padding field; a few bytes here")]
            padding: vec![0x33; 5],
            platform,
            platform_details: "Kourier 1.2 (linux)".to_owned(),
            app_version: "1.2".to_owned(),
            label: label.to_owned(),
        }
        .encode_to_vec()
    };
    let keys = phone.keys().clone();
    let m = w.mediator().clone();
    let shown = |phone: &mut Phone, sealed: Vec<u8>| {
        assert!(
            m.set_device_info(&keys.path_public, OTHER_ID, sealed),
            "the other device's slot is not there"
        );
        let rows = phone.linked_devices();
        assert_eq!(rows.len(), 1, "not one other device");
        rows[0].clone()
    };
    let desktop = shown(
        &mut phone,
        seal_nonce_ahead(
            &keys.device_info,
            &[1; 24],
            &info(Platform::Desktop as i32, "Kourier (host)"),
        ),
    );
    assert!(
        desktop.platform == Platform::Desktop
            && desktop.valid
            && desktop.title == "Kourier (host)"
            && desktop.details == "1.2 · Kourier 1.2 (linux)",
        "a DESKTOP info does not read as its label, version and details"
    );
    let empty_label = shown(
        &mut phone,
        seal_nonce_ahead(
            &keys.device_info,
            &[2; 24],
            &info(Platform::Desktop as i32, ""),
        ),
    );
    assert!(
        empty_label.valid && empty_label.title.is_empty(),
        "an empty label does not stay empty"
    );
    let invalid = [
        (
            "UNSPECIFIED",
            seal_nonce_ahead(
                &keys.device_info,
                &[3; 24],
                &info(Platform::Unspecified as i32, "Kourier (host)"),
            ),
        ),
        (
            "a platform number the app lacks",
            seal_nonce_ahead(&keys.device_info, &[4; 24], &info(9, "Kourier (host)")),
        ),
        (
            "a box under DGRK",
            seal_nonce_ahead(
                &keys.reflect,
                &[5; 24],
                &info(Platform::Desktop as i32, "Kourier (host)"),
            ),
        ),
        ("no box", vec![7; 30]),
        ("nothing", Vec::new()),
    ];
    for (what, sealed) in invalid {
        let row = shown(&mut phone, sealed);
        assert!(
            row.platform == Platform::Unspecified
                && !row.valid
                && row.title == INVALID_TITLE
                && row.details == INVALID_DETAILS,
            "{what}: not shown as a device whose information could not be obtained"
        );
    }
    other.disconnect();
    let rows = phone.linked_devices();
    assert!(
        rows.len() == 1 && !rows[0].active,
        "a device that left is still currently active"
    );
    assert!(
        !w.mediator()
            .set_device_info(&keys.path_public, 0x77, vec![1]),
        "a slot that is not there was found"
    );
    w.violations().assert_empty();
}
