//! The fake's LEADER side (TH6 L2a): a peer that sends to the user (`support/mock/peer.rs` `Letter`,
//! `Peer::send`), the phone's follower view and its takeover (`support/mock/phone.rs`), `GetDevicesInfo`
//! (`support/mock/d2m.rs`) and THE GUARD ORACLE (`support/mock/leader.rs`). "Kourier" here is the REAL
//! patched libthreema D2M + CSP client of `csp.rs` and the test plays the leader's steps by hand: it opens
//! a letter with libthreema's own P5 boxes (so the fake's seal is checked against a second
//! implementation), reflects the `IncomingMessage` it builds and acks. No runtime, no `unshare`. Every
//! test ends with the violations check; the oracle's own refusals name the rules they expect.

use std::time::Duration;

use libthreema::common::keys::{ClientKey, PublicKey};
use libthreema::common::{D2xDeviceId, Nonce};
use libthreema::csp::payload::{IncomingPayload, OutgoingPayload};
use libthreema::d2m::payload::{
    IncomingPayload as D2mIncoming, OutgoingPayload as D2mOutgoing, Reflect, ReflectFlags,
};
use libthreema::protobuf::d2m::devices_info::augmented_device_info::ConnectionState;
use libthreema::protobuf::{csp_e2e, d2d};
use prost::Message as _;

use super::csp::{Device, ME, PEER, Setup, ack, logged_in, messages, names, pump, scene, send};
use super::d2m::{Group, SERVER_GROUP, existing, new_device};
use crate::support::mock::crypto::seal_nonce_ahead;
use crate::support::mock::csp::flag;
use crate::support::mock::d2m::EPHEMERAL;
use crate::support::mock::leader::{Posted, Recorded};
use crate::support::mock::peer::{HEAD, Letter, LetterMeta, NONCE};
use crate::support::mock::phone::{LedOutcome, Phone, open_envelope};
use crate::support::mock::{MockWorld, START_MS};

/// Kourier's device id and the phone's.
const KOURIER: u64 = 1;
const PHONE: u64 = 2;
const OTHER: &str = "OTHER001";

/// A letter opened with libthreema's P5 boxes: its metadata (if it has one), the container's type and
/// the data without its padding, and the padded length.
struct Read {
    meta: Option<csp_e2e::MessageMetadata>,
    ty: u8,
    body: Vec<u8>,
    padded: usize,
    nonce: [u8; 24],
}

/// Opens `bytes` (a `message-with-metadata-box` for `ME`) as a receiver does, with libthreema's
/// `CspE2eKey` (patch P5): `None` when a box does not open.
fn open_with_libthreema(w: &MockWorld, bytes: &[u8]) -> Option<Read> {
    let sender = std::str::from_utf8(&bytes[..8]).ok()?;
    let me = w.identity(ME)?;
    let peer = w.identity(sender)?;
    let key = ClientKey::from(me.ck.secret).derive_csp_e2e_key(&PublicKey::from(peer.ck.public))?;
    let meta_len = usize::from(u16::from_le_bytes([bytes[30], bytes[31]]));
    let nonce: [u8; 24] = bytes[HEAD + meta_len..HEAD + meta_len + NONCE]
        .try_into()
        .ok()?;
    let meta = if meta_len == 0 {
        None
    } else {
        let plain = key
            .decrypt_metadata(&Nonce(nonce), &bytes[HEAD..HEAD + meta_len])
            .ok()?;
        Some(csp_e2e::MessageMetadata::decode(plain.as_slice()).ok()?)
    };
    let plain = key
        .decrypt_message(&Nonce(nonce), &bytes[HEAD + meta_len + NONCE..])
        .ok()?;
    let (&ty, padded) = plain.split_first()?;
    // libthreema's rule (T-L4): only the last byte says how long the padding is.
    let pad = usize::from(*padded.last()?);
    Some(Read {
        meta,
        ty,
        body: padded[..padded.len() - pad].to_vec(),
        padded: padded.len(),
        nonce,
    })
}

/// The `IncomingMessage` envelope a leader reflects for `bytes` (`TP/md-d2d.proto:247-292`): the
/// sender, the id, the metadata's time (the head's x 1000 without metadata), the outer type, the
/// unpadded body, the CSP nonce.
fn incoming_of(w: &MockWorld, bytes: &[u8]) -> d2d::Envelope {
    let read = open_with_libthreema(w, bytes).expect("the letter opens");
    let head_ms = u64::from(u32::from_le_bytes(bytes[24..28].try_into().expect("4"))) * 1000;
    d2d::Envelope {
        #[expect(deprecated, reason = "the spec's padding field; empty here")]
        padding: vec![],
        device_id: KOURIER,
        protocol_version: d2d::ProtocolVersion::V03 as u32,
        content: Some(d2d::envelope::Content::IncomingMessage(
            d2d::IncomingMessage {
                sender_identity: String::from_utf8(bytes[..8].to_vec()).expect("an ID"),
                message_id: u64::from_le_bytes(bytes[16..24].try_into().expect("8")),
                created_at: read.meta.map_or(head_ms, |m| m.created_at),
                r#type: i32::from(read.ty),
                body: read.body,
                nonce: read.nonce.to_vec(),
            },
        )),
    }
}

/// Seals `envelope` under DGRK with a fresh nonce of the world (a new D2D nonce each call).
fn seal(w: &MockWorld, g: &Group, envelope: &d2d::Envelope) -> Vec<u8> {
    seal_nonce_ahead(&g.keys.reflect, &w.random(), &envelope.encode_to_vec())
}

/// Kourier's D2M leg writes a `Reflect`.
fn reflect(w: &MockWorld, d: &mut Device, reflect_id: u32, flags: u16, envelope: Vec<u8>) {
    let datagram = d
        .d2m
        .proto
        .create_payload(D2mOutgoing::Reflect(Reflect {
            flags: ReflectFlags(flags),
            reflect_id,
            envelope,
        }))
        .expect("the client encodes the reflect")
        .outgoing_datagram
        .expect("a datagram");
    w.mediator().receive(d.d2m.conn(), datagram.0);
    pump(w, d);
}

/// Kourier's D2M leg writes `GetDevicesInfo` and gets the answer, as libthreema decodes it.
fn devices(w: &MockWorld, d: &mut Device) -> Vec<(u64, Option<ConnectionState>)> {
    let datagram = d
        .d2m
        .proto
        .create_payload(D2mOutgoing::GetDevicesInfo)
        .expect("encodes")
        .outgoing_datagram
        .expect("a datagram");
    w.mediator().receive(d.d2m.conn(), datagram.0);
    pump(w, d);
    let Some(D2mIncoming::DevicesInfo(info)) = d.d2m.payloads.pop() else {
        panic!("no DevicesInfo came");
    };
    let mut out: Vec<(u64, Option<ConnectionState>)> = info
        .0
        .iter()
        .map(|(D2xDeviceId(id), a)| (*id, a.connection_state))
        .collect();
    out.sort_by_key(|(id, _)| *id);
    out
}

/// The reflect-ids of the `ReflectAck`s Kourier got, in order.
fn reflect_acks(d: &Device) -> Vec<u32> {
    d.d2m
        .payloads
        .iter()
        .filter_map(|p| match p {
            D2mIncoming::ReflectAck(a) => Some(a.reflect_id),
            _ => None,
        })
        .collect()
}

/// Whether Kourier's D2M leg was promoted.
fn promoted(d: &Device) -> bool {
    d.d2m
        .payloads
        .iter()
        .any(|p| matches!(p, D2mIncoming::RolePromotedToLeader))
}

/// The struct of every incoming message Kourier got, in order.
fn boxes(d: &Device) -> Vec<Vec<u8>> {
    d.payloads
        .iter()
        .filter_map(|p| match p {
            IncomingPayload::MessageWithMetadataBox(m) => Some(m.bytes.clone()),
            _ => None,
        })
        .collect()
}

/// The record's events as short names with the message or reflect id.
fn record(w: &MockWorld) -> Vec<(&'static str, u64, u64)> {
    w.mediator()
        .leader_log()
        .events
        .iter()
        .map(|e| match e {
            Recorded::Delivered {
                conn, message_id, ..
            } => ("delivered", *conn, *message_id),
            Recorded::CspAck {
                conn, message_id, ..
            } => ("csp-ack", *conn, *message_id),
            Recorded::Reflect {
                conn, reflect_id, ..
            } => ("reflect", *conn, u64::from(*reflect_id)),
            Recorded::ReflectAck { conn, reflect_id } => {
                ("reflect-ack", *conn, u64::from(*reflect_id))
            }
        })
        .collect()
}

/// The world, the group, the phone that has a slot and is OFFLINE (it led for a moment at its first
/// login: the first device of a group leads), Kourier logged in as the LEADER (the phone gone), and
/// how Kourier logs in.
fn kourier_leads(seed: u64) -> (MockWorld, Group, Phone, Device, Setup) {
    let (w, g, s) = scene(seed, true);
    w.add_identity(OTHER);
    let mut phone = w.phone(&g.dgk, SERVER_GROUP, PHONE);
    phone.connect();
    assert!(phone.leads(), "the first device of a group leads");
    phone.disconnect();
    w.mediator().advance(Duration::from_secs(1));
    let d = logged_in(&w, &g, new_device(KOURIER), &s);
    assert!(
        promoted(&d) && w.mediator().is_leader(d.d2m.conn()),
        "Kourier, alone online, leads"
    );
    (w, g, phone, d, s)
}

/// A peer's letter is a real `message-with-metadata-box` (libthreema's own boxes open it to the text, the
/// nickname, the id and the time), the chat server holds it for the user until the leader unblocks,
/// then delivers it; the leader reflects it, waits for the reflect-ack, acks it; the phone, back online,
/// sees ONE `IncomingMessage` equal to the letter; the oracle finds the order kept.
#[test]
fn a_peers_letter_reaches_the_leader_after_its_unblock_and_the_phone_through_its_reflection() {
    let (w, g, mut phone, mut d, _) = kourier_leads(901);
    let posted = w
        .peer(PEER)
        .send(&Letter::text(ME, 0x1001, "hello").nickname("Pat"));
    assert!(posted.queued, "the chat server queued the letter");
    assert_eq!(w.mediator().csp_queue_len(ME), 1);
    assert!(messages(&d).is_empty(), "a letter before the unblock");
    send(&w, &mut d, OutgoingPayload::UnblockIncomingMessages);
    assert_eq!(names(&d), vec!["message", "queue-send-complete"]);
    assert!(
        boxes(&d) == vec![posted.bytes.clone()],
        "the leader got the struct as posted"
    );
    let read = open_with_libthreema(&w, &posted.bytes).expect("libthreema opens the letter");
    let meta = read.meta.expect("a metadata box");
    assert_eq!(meta.message_id, 0x1001, "the metadata's id");
    assert_eq!(meta.created_at, START_MS + 1000, "the world's clock");
    assert_eq!(meta.nickname.as_deref(), Some("Pat"), "the nickname");
    assert_eq!((read.ty, read.body.as_slice()), (0x01, b"hello".as_slice()));
    assert!(read.padded >= 32, "the padded data has at least 32 bytes");
    assert_eq!(
        u32::from_le_bytes(posted.bytes[24..28].try_into().expect("4")),
        u32::try_from(meta.created_at / 1000).expect("seconds"),
        "the head's created-at is the metadata's in seconds"
    );
    assert_eq!(&posted.bytes[32..HEAD], &[0u8; 32], "no legacy nickname");

    // The leader's steps: reflect, its ack, then the CSP ack.
    let envelope = incoming_of(&w, &posted.bytes);
    reflect(&w, &mut d, 1, 0, seal(&w, &g, &envelope));
    assert_eq!(reflect_acks(&d), vec![1], "the reflect-ack");
    ack(&w, &mut d, PEER, 0x1001);
    assert_eq!(
        w.mediator().csp_queue_len(ME),
        0,
        "acked: gone from the queue"
    );
    let conn = d.d2m.conn().0;
    assert_eq!(
        record(&w),
        vec![
            ("delivered", conn, 0x1001),
            ("reflect", conn, 1),
            ("reflect-ack", conn, 1),
            ("csp-ack", conn, 0x1001),
        ]
    );

    // The phone, back as a follower, gets the reflection from its queue and maps it once.
    phone.connect();
    assert!(!phone.leads(), "the phone comes back while Kourier leads");
    let seen = phone.incoming();
    assert_eq!(seen.len(), 1, "one reflection");
    let view = phone.view().clone();
    assert!(
        view.incoming_ids() == vec![(PEER.to_owned(), 0x1001)],
        "the phone's view"
    );
    let message = view.incoming()[0].clone();
    assert!(
        message.body == b"hello" && message.nonce == posted.nonce.to_vec(),
        "the body and the CSP nonce"
    );
    assert_eq!((message.r#type, message.created_at), (1, meta.created_at));
    assert_eq!(
        w.mediator().queue_len(&g.keys.path_public, PHONE),
        Some(0),
        "the phone acked it"
    );
    assert!(
        w.leader_violations(&g.keys, PHONE).is_empty(),
        "the oracle found a breach"
    );
    assert!(
        !format!("{posted:?}{:?}", w.mediator().leader_log()).contains(PEER),
        "a Debug printed an ID"
    );
    w.violations().assert_empty();
}

/// What the leader does not ack comes again at its next login (login-ack counts it), what it acked does
/// not; a letter that wants no ack (0x04) goes out once and is not kept.
#[test]
fn an_unacked_letter_comes_again_at_every_login_until_it_is_acked() {
    let (w, g, _phone, mut d, s) = kourier_leads(902);
    let mut peer = w.peer(PEER);
    let first = peer.send(&Letter::text(ME, 1, "one"));
    let second = peer.send(&Letter::text(ME, 2, "two"));
    let typing = peer.send(&Letter::new(ME, 3, 0x90, &[1]).flags(flag::NO_SERVER_ACK));
    send(&w, &mut d, OutgoingPayload::UnblockIncomingMessages);
    assert!(
        boxes(&d) == vec![first.bytes.clone(), second.bytes, typing.bytes],
        "the three in order"
    );
    reflect(
        &w,
        &mut d,
        1,
        0,
        seal(&w, &g, &incoming_of(&w, &first.bytes)),
    );
    ack(&w, &mut d, PEER, 1);
    let one = d.d2m.conn().0;
    w.mediator().client_close(d.d2m.conn(), 1000);

    let mut d = logged_in(&w, &g, existing(KOURIER), &s);
    assert!(promoted(&d), "Kourier leads again (the phone is offline)");
    assert_eq!(d.login.as_ref().map(|l| l.queued_messages), Some(1));
    send(&w, &mut d, OutgoingPayload::UnblockIncomingMessages);
    assert_eq!(names(&d), vec!["message", "queue-send-complete"]);
    assert!(
        messages(&d) == vec![(PEER.to_owned(), 2)],
        "only the one that was not acked"
    );
    let two = d.d2m.conn().0;
    let bytes = boxes(&d).remove(0);
    reflect(&w, &mut d, 1, 0, seal(&w, &g, &incoming_of(&w, &bytes)));
    ack(&w, &mut d, PEER, 2);
    assert_eq!(w.mediator().csp_queue_len(ME), 0);
    let delivered: Vec<(u64, u64)> = record(&w)
        .into_iter()
        .filter(|(kind, ..)| *kind == "delivered")
        .map(|(_, conn, id)| (conn, id))
        .collect();
    assert_eq!(delivered, vec![(one, 1), (one, 2), (one, 3), (two, 2)]);
    assert!(w.leader_violations(&g.keys, PHONE).is_empty(), "the oracle");
    w.violations().assert_empty();
}

/// The phone online as a follower: a reflection that comes twice (the same bytes under two reflect-ids,
/// a resend after a lost ack) is viewed once and the second discarded by its D2D nonce, both acked; an
/// ephemeral reflection (typing) is viewed and not acked; one that does not open is the leader's breach.
#[test]
fn the_phone_views_a_reflection_once_and_discards_its_second_copy_by_the_d2d_nonce() {
    let (w, g, mut phone, mut d, _) = kourier_leads(903);
    phone.connect();
    assert!(
        !phone.leads(),
        "a device that comes online while another leads follows"
    );
    let posted = w.peer(PEER).send(&Letter::text(ME, 7, "twice"));
    send(&w, &mut d, OutgoingPayload::UnblockIncomingMessages);
    let sealed = seal(&w, &g, &incoming_of(&w, &posted.bytes));
    reflect(&w, &mut d, 1, 0, sealed.clone());
    reflect(&w, &mut d, 2, 0, sealed);
    ack(&w, &mut d, PEER, 7);
    let typing = d2d::Envelope {
        #[expect(deprecated, reason = "the spec's padding field; empty here")]
        padding: vec![],
        device_id: KOURIER,
        protocol_version: d2d::ProtocolVersion::V03 as u32,
        content: Some(d2d::envelope::Content::IncomingMessage(
            d2d::IncomingMessage {
                sender_identity: PEER.to_owned(),
                message_id: 8,
                created_at: START_MS,
                r#type: 0x90,
                body: vec![1],
                nonce: vec![],
            },
        )),
    };
    reflect(&w, &mut d, 3, EPHEMERAL, seal(&w, &g, &typing));
    assert_eq!(reflect_acks(&d), vec![1, 2], "no ack for the ephemeral one");

    let seen = phone.incoming();
    assert_eq!(
        seen.iter()
            .map(|v| (v.ephemeral, v.envelope.content.is_some()))
            .collect::<Vec<_>>(),
        vec![(false, true), (true, true)],
        "the text once, then the typing"
    );
    let view = phone.view().clone();
    assert_eq!(view.replays, 1, "the second copy discarded by its nonce");
    assert_eq!(view.kinds(), vec!["incoming_message", "incoming_message"]);
    assert_eq!(
        w.mediator().queue_len(&g.keys.path_public, PHONE),
        Some(0),
        "both copies acked"
    );
    assert!(w.leader_violations(&g.keys, PHONE).is_empty(), "the oracle");
    w.violations().assert_empty();

    // A reflection the device group's key does not open.
    reflect(
        &w,
        &mut d,
        4,
        0,
        seal_nonce_ahead(&[7; 32], &w.random(), b"not an envelope"),
    );
    assert!(phone.incoming().is_empty(), "nothing new in the view");
    assert_eq!(w.violations().rules(), vec!["phone-envelope-does-not-open"]);
}

/// THE GUARD ORACLE refuses what it is for: a message acked with no reflect, an ack before its
/// reflect-ack (the reflect-acks held back), one message reflected under two D2D nonces, a reflection
/// whose body is not the letter's; and it takes what it should: a declared discard acked unreflected (by
/// the letter or by the test), a held message never acked, the phone's own reflects and acks.
#[test]
fn the_guard_oracle_names_each_breach_of_the_leaders_order() {
    let (w, g, mut phone, mut d, _) = kourier_leads(904);
    let mut peer = w.peer(PEER);
    let letters: Vec<Posted> = (1..=7u64)
        .map(|id| {
            let letter = Letter::text(ME, id, "x");
            peer.send(&if id == 5 { letter.broken_box() } else { letter })
        })
        .collect();
    w.mediator().declare_discard(PEER, 6);
    send(&w, &mut d, OutgoingPayload::UnblockIncomingMessages);
    assert_eq!(boxes(&d).len(), 7, "all seven delivered");

    // 1: acked unreflected.
    ack(&w, &mut d, PEER, 1);
    // 2: the reflect-acks held back, the CSP ack before the release.
    w.mediator().knobs(|k| k.withhold_reflect_acks = true);
    reflect(
        &w,
        &mut d,
        1,
        0,
        seal(&w, &g, &incoming_of(&w, &letters[1].bytes)),
    );
    assert!(reflect_acks(&d).is_empty(), "the reflect-ack is held");
    ack(&w, &mut d, PEER, 2);
    assert_eq!(w.mediator().release_reflect_acks(d.d2m.conn()), 1);
    w.mediator().knobs(|k| k.withhold_reflect_acks = false);
    // 3: two seals of one message.
    let three = incoming_of(&w, &letters[2].bytes);
    reflect(&w, &mut d, 2, 0, seal(&w, &g, &three));
    reflect(&w, &mut d, 3, 0, seal(&w, &g, &three));
    ack(&w, &mut d, PEER, 3);
    // 4: another body than the letter's.
    let mut four = incoming_of(&w, &letters[3].bytes);
    if let Some(d2d::envelope::Content::IncomingMessage(m)) = four.content.as_mut() {
        m.body = b"y".to_vec();
    }
    reflect(&w, &mut d, 4, 0, seal(&w, &g, &four));
    ack(&w, &mut d, PEER, 4);
    // 5 (a broken box: the letter's discard) and 6 (declared by the test): acked unreflected, fine.
    ack(&w, &mut d, PEER, 5);
    ack(&w, &mut d, PEER, 6);
    // 7: held (never acked).
    assert_eq!(
        w.mediator().csp_queue_len(ME),
        1,
        "the held one stays queued"
    );
    // The phone online reflects and acks nothing of the user's queue; its own reflect is not the client's.
    phone.connect();
    phone.reflect(&incoming_of(&w, &letters[6].bytes));
    assert_eq!(
        w.leader_violations(&g.keys, PHONE),
        vec![
            "leader-acked-unreflected",
            "leader-ack-before-reflect-ack",
            "leader-reflected-twice",
            "leader-reflect-differs",
        ]
    );
    assert_eq!(
        w.violations().rules(),
        vec![
            "leader-acked-unreflected",
            "leader-ack-before-reflect-ack",
            "leader-reflected-twice",
            "leader-reflect-differs",
        ]
    );
}

/// TAKEOVER and T-H1's step-aside under the mediator's leader rule: Kourier leads with a letter it holds;
/// the phone comes online and FOLLOWS (no demotion while Kourier's socket stands); `GetDevicesInfo` shows
/// both connected (libthreema decodes the answer); Kourier closes once and the phone is promoted; Kourier
/// comes back a follower and may not unblock; the letter waits at the chat server; the phone goes and
/// Kourier, the longest connected, leads again and gets the letter again; the phone is listed with its
/// disconnect.
#[test]
fn the_phone_takes_the_lead_when_kouriers_socket_ends_and_kourier_comes_back_a_follower() {
    let (w, g, mut phone, mut d, s) = kourier_leads(905);
    let kourier_since = START_MS + 1000;
    let posted = w.peer(PEER).send(&Letter::text(ME, 0x51, "held"));
    send(&w, &mut d, OutgoingPayload::UnblockIncomingMessages);
    assert!(boxes(&d) == vec![posted.bytes.clone()], "delivered");
    // Held: neither reflected nor acked.
    w.mediator().advance(Duration::from_secs(2));
    phone.connect();
    let phone_since = START_MS + 3000;
    assert!(
        !phone.leads(),
        "the phone follows while Kourier's socket stands"
    );
    assert!(w.mediator().is_leader(d.d2m.conn()), "no demotion");
    assert_eq!(
        devices(&w, &mut d),
        vec![
            (
                KOURIER,
                Some(ConnectionState::ConnectedSince(kourier_since))
            ),
            (PHONE, Some(ConnectionState::ConnectedSince(phone_since))),
        ]
    );

    // The step-aside: Kourier closes once; the phone, connected, leads.
    w.mediator().client_close(d.d2m.conn(), 1000);
    assert!(phone.leads(), "the phone was promoted");
    let mut d = logged_in(&w, &g, existing(KOURIER), &s);
    assert!(
        !promoted(&d) && !w.mediator().is_leader(d.d2m.conn()),
        "Kourier comes back a follower"
    );
    send(&w, &mut d, OutgoingPayload::UnblockIncomingMessages);
    assert!(boxes(&d).is_empty(), "a follower got a letter");
    assert_eq!(w.mediator().csp_queue_len(ME), 1, "the letter waits");

    // The phone goes: Kourier, connected longest, is promoted and gets the letter again.
    w.mediator().advance(Duration::from_secs(4));
    phone.disconnect();
    pump(&w, &mut d);
    assert!(
        promoted(&d) && w.mediator().is_leader(d.d2m.conn()),
        "Kourier leads again"
    );
    send(&w, &mut d, OutgoingPayload::UnblockIncomingMessages);
    assert!(boxes(&d) == vec![posted.bytes], "delivered again");
    assert_eq!(
        devices(&w, &mut d),
        vec![
            (
                KOURIER,
                Some(ConnectionState::ConnectedSince(START_MS + 3000))
            ),
            (
                PHONE,
                Some(ConnectionState::LastDisconnectAt(START_MS + 7000))
            ),
        ]
    );
    assert!(w.leader_violations(&g.keys, PHONE).is_empty(), "the oracle");
    assert_eq!(w.violations().rules(), vec!["csp-unblock-from-a-follower"]);
}

/// Each way a letter can be bent is in its bytes: another receiver in the head (queued for the user all
/// the same), a box that does not open (libthreema refuses it), a metadata id that differs, a reused
/// nonce, no metadata (the head's time), a type the pinned spec does not know, a padding of non-uniform
/// bytes whose last byte says its length (libthreema's rule), a legacy nickname, flags; the bent ones are
/// the letter's declared discards; all are delivered in order.
#[test]
fn a_letter_can_be_bent_each_way_the_receive_steps_discard() {
    let (w, _g, _phone, mut d, _) = kourier_leads(906);
    let mut peer = w.peer(PEER);
    let plain = peer.send(&Letter::text(ME, 10, "plain"));
    let receiver = peer.send(&Letter::text(ME, 11, "r").wrong_receiver(OTHER));
    let broken = peer.send(&Letter::text(ME, 12, "b").broken_box());
    let meta_id = peer.send(&Letter::text(ME, 13, "m").meta_id(99));
    let reused = peer.send(&Letter::text(ME, 14, "n").reused_nonce(plain.nonce));
    let absent = peer.send(
        &Letter::text(ME, 15, "a")
            .meta(LetterMeta::Absent)
            .created_ms(START_MS + 4_321),
    );
    let unknown = peer.send(&Letter::new(ME, 16, 0x7e, &[1, 2, 3]));
    let padding = peer.send(&Letter::text(ME, 17, "p").padding(&[9, 8, 7, 4]));
    let legacy = peer.send(
        &Letter::text(ME, 18, "l")
            .legacy([b'x'; 32])
            .flags(flag::NO_SERVER_ACK | 0x80),
    );
    let all = [
        &plain, &receiver, &broken, &meta_id, &reused, &absent, &unknown, &padding, &legacy,
    ];
    assert_eq!(
        all.iter().map(|p| p.discard).collect::<Vec<_>>(),
        vec![false, true, true, true, true, false, false, false, false],
        "the bent ones are declared discards"
    );
    assert!(
        &receiver.bytes[8..16] == OTHER.as_bytes() && receiver.to == ME,
        "another receiver in the head, queued for the user"
    );
    assert!(
        open_with_libthreema(&w, &broken.bytes).is_none(),
        "the broken box opened"
    );
    let meta = open_with_libthreema(&w, &meta_id.bytes)
        .and_then(|r| r.meta)
        .expect("opens");
    assert_eq!(meta.message_id, 99, "the metadata's id");
    assert_eq!(reused.nonce, plain.nonce, "the reused nonce");
    let read = open_with_libthreema(&w, &absent.bytes).expect("opens");
    assert!(
        read.meta.is_none() && absent.bytes[30..32] == [0, 0],
        "no metadata"
    );
    assert_eq!(
        absent.created_ms,
        (START_MS + 4_321) / 1000 * 1000,
        "the head's time"
    );
    let read = open_with_libthreema(&w, &unknown.bytes).expect("opens");
    assert_eq!((read.ty, read.body), (0x7e, vec![1, 2, 3]));
    let read = open_with_libthreema(&w, &padding.bytes).expect("opens");
    assert_eq!(read.body, b"p", "the last byte says the padding's length");
    assert_eq!(&legacy.bytes[32..HEAD], &[b'x'; 32], "the legacy nickname");
    assert_eq!(legacy.bytes[28], flag::NO_SERVER_ACK | 0x80, "the flags");

    send(&w, &mut d, OutgoingPayload::UnblockIncomingMessages);
    assert!(
        boxes(&d) == all.iter().map(|p| p.bytes.clone()).collect::<Vec<_>>(),
        "every letter delivered in order"
    );
    w.violations().assert_empty();
}

/// TH6 L2b, THE PHONE TAKES THE QUEUE (`Phone::lead_csp_queue`): Kourier leads, gets four messages, reflects the first
/// (its reflect-ack comes) and its socket ends before any `message-ack` (window L3); the peer then sends the first id
/// again under a fresh nonce. The phone, offline, takes nothing; online and alone it leads and takes the user's queue:
/// the first it has VIEWED (Kourier's reflection carries its CSP nonce) is acked and discarded by its nonce (step 2),
/// the second reflected as an `IncomingMessage` equal to the letter and acked, the bent one and the opaque one (it
/// cannot open them) acked and discarded, the resend of the first id discarded by id (step 14). The queue is empty, a
/// second take finds nothing, Kourier's slot holds the phone's reflection, and the oracle (which skips the phone) finds
/// no breach of Kourier's. ONE `Peer` sends both letters: two `w.peer(id)` handles draw the SAME nonces.
#[test]
fn the_leading_phone_takes_the_queue_and_reflects_only_what_it_has_not_seen() {
    let (w, g, mut phone, mut d, _) = kourier_leads(907);
    send(&w, &mut d, OutgoingPayload::UnblockIncomingMessages);
    let mut peer = w.peer(PEER);
    let one = peer.send(&Letter::text(ME, 0x2001, "one"));
    let two = peer.send(&Letter::text(ME, 0x2002, "two"));
    w.peer(OTHER)
        .send(&Letter::text(ME, 0x2003, "bent").broken_box());
    assert!(
        w.mediator()
            .csp_incoming(ME, OTHER, 0x2004, 0, b"an opaque box"),
        "queued"
    );
    pump(&w, &mut d);
    assert_eq!(boxes(&d).len(), 4, "all four delivered to Kourier");
    let envelope = incoming_of(&w, &one.bytes);
    reflect(&w, &mut d, 1, 0, seal(&w, &g, &envelope));
    assert_eq!(reflect_acks(&d), vec![1], "Kourier's reflect-ack");
    assert!(
        phone.lead_csp_queue(ME).is_none(),
        "an offline phone takes nothing"
    );
    w.mediator().client_close(d.d2m.conn(), 1000);
    let again = peer.send(&Letter::text(ME, 0x2001, "one again"));
    assert!(again.nonce != one.nonce, "a fresh nonce");
    phone.connect();
    assert!(phone.leads(), "alone online, the phone leads");
    let pc = phone.conn().expect("connected").0;

    let led = phone.lead_csp_queue(ME).expect("the phone leads");
    let outcomes: Vec<(u64, LedOutcome)> = led.iter().map(|l| (l.message_id, l.outcome)).collect();
    assert_eq!(
        outcomes,
        vec![
            (0x2001, LedOutcome::NonceUsed),
            (0x2002, LedOutcome::Reflected { reflect_id: 1 }),
            (0x2003, LedOutcome::Discarded),
            (0x2004, LedOutcome::Unopened),
            (0x2001, LedOutcome::Seen),
        ]
    );
    assert_eq!(w.mediator().csp_queue_len(ME), 0, "every message acked");
    assert_eq!(phone.lead_csp_queue(ME), Some(Vec::new()), "nothing left");
    let phones: Vec<(&'static str, u64, u64)> = record(&w)
        .into_iter()
        .filter(|(_, conn, _)| *conn == pc)
        .collect();
    assert_eq!(
        phones,
        vec![
            ("delivered", pc, 0x2001),
            ("delivered", pc, 0x2002),
            ("delivered", pc, 0x2003),
            ("delivered", pc, 0x2004),
            ("delivered", pc, 0x2001),
            ("csp-ack", pc, 0x2001),
            ("reflect", pc, 1),
            ("reflect-ack", pc, 1),
            ("csp-ack", pc, 0x2002),
            ("csp-ack", pc, 0x2003),
            ("csp-ack", pc, 0x2004),
            ("csp-ack", pc, 0x2001),
        ]
    );
    let reflected = w.mediator().reflects().pop().expect("the phone's reflect");
    let opened = open_envelope(&g.keys, &reflected.bytes).expect("it opens with DGRK");
    let Some(d2d::envelope::Content::IncomingMessage(message)) = opened.content else {
        panic!("the phone reflected no IncomingMessage");
    };
    assert!(
        message.sender_identity == PEER
            && message.message_id == 0x2002
            && message.body == b"two"
            && message.nonce == two.nonce.to_vec()
            && message.created_at == two.created_ms
            && message.r#type == 1
            && opened.device_id == PHONE,
        "the phone's reflection is the letter"
    );
    assert_eq!(
        w.mediator().queue_len(&g.keys.path_public, KOURIER),
        Some(1),
        "Kourier's slot holds the phone's reflection"
    );
    assert!(
        w.leader_violations(&g.keys, PHONE).is_empty(),
        "the oracle found a breach"
    );
    w.violations().assert_empty();
}
