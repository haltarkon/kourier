//! The fake chat server inside the mediator's Proxy payloads (`support/mock/csp.rs`), against the
//! REAL `D2mProtocol` + `CspProtocol` of the patched libthreema: proofs 10, 11, 12 (chunks), 13 and
//! the CSP half of 14 of lane plan §5.2. The CSP client's frames go out in D2M Proxy payloads and
//! the fake's Proxy payloads go into the CSP client, all in this thread.

use std::time::Duration;

use kourier_threema::close::{CloseError, Stop, Verdict, verdict};
use libthreema::common::keys::{ClientKey, PublicKey};
use libthreema::common::{
    ClientInfo, CspDeviceId, DeviceCookie, MessageFlags, MessageId, ThreemaId,
};
use libthreema::csp::payload::{
    ConnectionIdleTimeout, DeletePushNotificationToken, EchoPayload, IncomingPayload, LoginAckData,
    MessageAck, MessageWithMetadataBox, OutgoingPayload, PushNotificationToken,
};
use libthreema::csp::{CspProtocol, CspProtocolContext, CspProtocolContextInit, CspStateUpdate};
use libthreema::d2m::D2mStateUpdate;
use libthreema::d2m::payload::{
    IncomingPayload as D2mIncoming, OutgoingPayload as D2mOutgoing, Reflect, ReflectFlags,
};

use super::d2m::{Client, Group, Opts, client, existing, group, new_device};
use super::peer::Wire;
use crate::support::mock::crypto::{CspNonces, csp_nonce, hsalsa_shared, open, vouch_from_secrets};
use crate::support::mock::csp::{
    CHAT_SERVER_CLOSED, DEFAULT_CSP_IDLE_TIMEOUT, Evil, MAX_FRAME, flag, kind as csp_kind,
    open_login_ack, seal_frame,
};
use crate::support::mock::d2m::{DEFAULT_IDLE_TIMEOUT, Out, datagram, kind};
use crate::support::mock::peer::{Discard, Opened};
use crate::support::mock::{MockWorld, START_MS, TestIdentity, world};

const ME: &str = "KOURIER1";
const PEER: &str = "PEER0001";

/// How the CSP client of a test logs in.
#[derive(Clone)]
pub(super) struct Setup {
    /// The Threema ID it logs in as (`ME`, or a joined device's, TH4 J2).
    pub(super) identity: String,
    /// The client key the client uses (the table's, unless a test lies).
    pub(super) ck_secret: [u8; 32],
    /// The permanent server keys the client trusts, primary first.
    pub(super) server_keys: Vec<[u8; 32]>,
    pub(super) cookie: Option<[u8; 16]>,
    pub(super) csp_device_id: Option<u64>,
    /// Send the client-hello in two Proxy payloads.
    pub(super) split_hello: bool,
}

fn setup(w: &MockWorld, me: &TestIdentity) -> Setup {
    Setup {
        identity: me.id.clone(),
        ck_secret: me.ck.secret,
        server_keys: vec![w.server_key().public],
        cookie: Some([0x5c; 16]),
        csp_device_id: Some(0x0123_4567_89ab_cdef),
        split_hello: false,
    }
}

/// One device: the real D2M client, the real CSP client inside it, and what each saw.
pub(super) struct Device {
    d2m: Client,
    csp: CspProtocol,
    login: Option<LoginAckData>,
    payloads: Vec<IncomingPayload>,
    csp_failed: bool,
    /// The fake's Proxy payloads, as they came (for the independent checks).
    raw: Vec<Vec<u8>>,
}

impl Device {
    pub(super) fn logged_in(&self) -> bool {
        self.login.is_some() && !self.csp_failed && self.d2m.closed.is_none()
    }
}

fn csp_client(s: &Setup) -> (CspProtocol, Vec<u8>) {
    let context = CspProtocolContext::try_from(CspProtocolContextInit {
        permanent_server_keys: s.server_keys.iter().map(|k| PublicKey::from(*k)).collect(),
        user_identity: ThreemaId::try_from(s.identity.as_str()).expect("the test's ID"),
        client_key: ClientKey::from(s.ck_secret),
        client_info: ClientInfo::Libthreema,
        device_cookie: s.cookie.map(DeviceCookie),
        csp_device_id: s.csp_device_id.map(CspDeviceId),
    })
    .expect("a CSP context");
    let (proto, hello) = CspProtocol::new(context);
    (proto, hello.0)
}

/// Sends CSP bytes of the client in one D2M Proxy payload.
fn proxy(w: &MockWorld, d: &mut Device, bytes: Vec<u8>) {
    match d.d2m.proto.create_payload(D2mOutgoing::Proxy(bytes)) {
        Ok(i) => {
            let datagram = i.outgoing_datagram.expect("a datagram");
            w.mediator().receive(d.d2m.conn(), datagram.0);
        }
        Err(_) => d.d2m.failed = true,
    }
}

/// Moves bytes every way (fake -> D2M client -> CSP client and back) until nothing moves.
fn pump(w: &MockWorld, d: &mut Device) {
    let m = w.mediator();
    let conn = d.d2m.conn();
    loop {
        let mut sent = false;
        // A close comes after what was sent before it: both clients read that first.
        let mut closing = None;
        for out in m.take(conn) {
            match out {
                Out::Datagram(datagram) => {
                    if d.d2m.proto.add_datagrams(vec![datagram]).is_err() {
                        d.d2m.failed = true;
                    }
                }
                Out::Close(code) => closing = Some(code),
            }
        }
        while !d.d2m.failed && d.d2m.closed.is_none() {
            match d.d2m.proto.poll() {
                Ok(Some(i)) => {
                    if let Some(D2mStateUpdate::PostHandshake(info)) = i.state_update {
                        d.d2m.post_handshake = Some(info);
                    }
                    match i.incoming_payload {
                        Some(D2mIncoming::Proxy(bytes)) => {
                            if d.csp.add_chunks(&[&bytes]).is_err() {
                                d.csp_failed = true;
                            }
                            d.raw.push(bytes);
                        }
                        Some(other) => d.d2m.payloads.push(other),
                        None => {}
                    }
                    if let Some(datagram) = i.outgoing_datagram {
                        m.receive(conn, datagram.0);
                        sent = true;
                    }
                }
                Ok(None) => break,
                Err(_) => d.d2m.failed = true,
            }
        }
        while !d.csp_failed && d.d2m.closed.is_none() {
            match d.csp.poll() {
                Ok(Some(i)) => {
                    if let Some(CspStateUpdate::PostHandshake(data)) = i.state_update {
                        d.login = Some(data);
                    }
                    if let Some(p) = i.incoming_payload {
                        d.payloads.push(p);
                    }
                    if let Some(frame) = i.outgoing_frame {
                        proxy(w, d, frame.0);
                        sent = true;
                    }
                }
                Ok(None) => break,
                Err(_) => d.csp_failed = true,
            }
        }
        if let Some(code) = closing {
            d.d2m.closed = Some(code);
        }
        if !sent {
            return;
        }
    }
}

/// Connects a device and sends its CSP client-hello at once (before the D2M handshake ended, as
/// Desktop does), then runs both handshakes as far as they go.
pub(super) fn connect(w: &MockWorld, g: &Group, o: Opts, s: &Setup) -> Device {
    let mut d2m = client(g.dgk, o);
    d2m.conn = Some(
        w.mediator()
            .connect(&d2m.url)
            .expect("the fake took the URL"),
    );
    let (csp, hello) = csp_client(s);
    let mut d = Device {
        d2m,
        csp,
        login: None,
        payloads: Vec::new(),
        csp_failed: false,
        raw: Vec::new(),
    };
    if s.split_hello {
        let (a, b) = hello.split_at(17);
        proxy(w, &mut d, a.to_vec());
        proxy(w, &mut d, b.to_vec());
    } else {
        proxy(w, &mut d, hello);
    }
    pump(w, &mut d);
    d
}

#[track_caller]
fn logged_in(w: &MockWorld, g: &Group, o: Opts, s: &Setup) -> Device {
    let d = connect(w, g, o, s);
    assert!(d.logged_in(), "the CSP client did not finish its login");
    d
}

/// Sends one CSP payload of the client.
fn send(w: &MockWorld, d: &mut Device, p: OutgoingPayload) {
    let frame = d
        .csp
        .create_payload(&p)
        .expect("the client encodes the payload")
        .outgoing_frame
        .expect("a frame");
    proxy(w, d, frame.0);
    pump(w, d);
}

/// The CSP payload names a device got, in order.
fn names(d: &Device) -> Vec<&'static str> {
    d.payloads
        .iter()
        .map(|p| match p {
            IncomingPayload::EchoRequest(_) => "echo-request",
            IncomingPayload::EchoResponse(_) => "echo-response",
            IncomingPayload::MessageWithMetadataBox(_) => "message",
            IncomingPayload::MessageAck(_) => "message-ack",
            IncomingPayload::QueueSendComplete => "queue-send-complete",
            IncomingPayload::DeviceCookieChangeIndication => "device-cookie-change-indication",
            IncomingPayload::CloseError(_) => "close-error",
            IncomingPayload::ServerAlert(_) => "alert",
            IncomingPayload::UnknownPayload { .. } => "unknown",
        })
        .collect()
}

/// The (sender, message id) of the messages a device got, in order.
fn messages(d: &Device) -> Vec<(String, u64)> {
    d.payloads
        .iter()
        .filter_map(|p| match p {
            IncomingPayload::MessageWithMetadataBox(m) => {
                Some((m.sender_identity.to_string(), m.message_id.0))
            }
            _ => None,
        })
        .collect()
}

fn ack(w: &MockWorld, d: &mut Device, sender: &str, message_id: u64) {
    send(
        w,
        d,
        OutgoingPayload::MessageAck(MessageAck {
            sender_identity: ThreemaId::try_from(sender).expect("an ID"),
            message_id: MessageId(message_id),
        }),
    );
}

/// A world with Kourier's identity and the peer, one device group, leader mode as asked.
fn scene(seed: u64, leader_mode: bool) -> (MockWorld, Group, Setup) {
    let w = world(seed);
    let me = w.add_identity(ME);
    w.add_identity(PEER);
    w.mediator().knobs(|k| k.leader_mode = leader_mode);
    let g = group(&w);
    let s = setup(&w, &me);
    (w, g, s)
}

// --- Proof 10: the CSP handshake inside Proxy -------------------------------------------------

/// The real CSP client reaches login-ack through the mediator's Proxy payloads; the fake saw its
/// extensions (client info, device id, features as a mask, the cookie); the login-ack opens with
/// the fake's own crypto to 16 bytes (Desktop's only check), reserved zero, the time and queue.
#[test]
fn the_real_csp_client_logs_in_through_the_proxy() {
    let (w, g, s) = scene(201, false);
    let d = logged_in(&w, &g, new_device(1), &s);
    let conn = d.d2m.conn();
    assert!(w.mediator().csp_logged_in(conn), "the fake has no login");
    let info = w.mediator().csp_login(conn).expect("the login");
    assert!(
        info.client_info.as_deref().is_some_and(|c| !c.is_empty()),
        "no client info extension"
    );
    assert_eq!(info.csp_device_id, Some(0x0123_4567_89ab_cdef), "device id");
    assert!(info.sent_device_cookie, "no device cookie extension");
    assert!(info.unknown_extensions.is_empty(), "unknown extensions");
    assert_eq!(d.login.as_ref().map(|l| l.queued_messages), Some(0));
    // The independent check: server-hello (80 B) then login-ack (32 B), no chunking.
    assert_eq!(
        d.raw.iter().map(Vec::len).collect::<Vec<_>>(),
        vec![80, 32],
        "the fake's handshake structs"
    );
    let (key, sck) = w.mediator().csp_session(conn).expect("a session");
    let data = open_login_ack(&key, &sck, &d.raw[1]).expect("the login-ack opens with SSN 2");
    assert_eq!(data.len(), 16, "Desktop's rule: 16 bytes of login-ack data");
    assert_eq!(data[..4], [0; 4], "reserved");
    assert_eq!(
        u64::from_le_bytes(data[4..12].try_into().expect("8 bytes")),
        START_MS,
        "the virtual clock"
    );
    assert_eq!(
        u32::from_le_bytes(data[12..].try_into().expect("4 bytes")),
        0
    );
    assert!(
        d.payloads.is_empty(),
        "the fake sent payloads after the login"
    );
    w.violations().assert_empty();
}

/// A server key the client does not trust: the server-hello box does not open, the client stops.
#[test]
fn an_unknown_server_key_is_refused_by_the_client() {
    let (w, g, mut s) = scene(202, false);
    s.server_keys = vec![w.new_key_pair().public];
    let d = connect(&w, &g, new_device(1), &s);
    assert!(d.csp_failed, "the client took a server it does not know");
    assert!(d.login.is_none(), "a login");
    assert!(
        !w.mediator().csp_logged_in(d.d2m.conn()),
        "the fake logged in"
    );
    w.violations().assert_empty();
}

/// The primary key unknown, the server's key second: the client logs in (libthreema warns).
#[test]
fn a_fallback_server_key_is_accepted() {
    let (w, g, mut s) = scene(203, false);
    s.server_keys = vec![w.new_key_pair().public, w.server_key().public];
    let d = logged_in(&w, &g, new_device(1), &s);
    assert!(w.mediator().csp_logged_in(d.d2m.conn()), "the fake");
    w.violations().assert_empty();
}

/// `SCK == CCK`, and a repeated CCK that is not the client's: the client refuses both.
#[test]
fn an_evil_server_hello_is_refused_by_the_client() {
    for (seed, evil) in [(204, Evil::SckEqualsCck), (205, Evil::WrongRepeatedCck)] {
        let (w, g, s) = scene(seed, false);
        w.mediator().knobs(|k| k.csp.evil = evil);
        let d = connect(&w, &g, new_device(1), &s);
        assert!(
            d.csp_failed,
            "the client took an evil server-hello ({evil:?})"
        );
        assert!(d.login.is_none(), "a login ({evil:?})");
        w.violations().assert_empty();
    }
}

/// A client key that is not the identity's: the vouch is wrong, the server drops the connection
/// (4000) and records it; an identity the server does not know likewise.
#[test]
fn a_bad_vouch_or_an_unknown_identity_is_refused_by_the_server() {
    let (w, g, mut s) = scene(206, false);
    s.ck_secret = w.new_key_pair().secret;
    let d = connect(&w, &g, new_device(1), &s);
    assert_eq!(
        d.d2m.closed,
        Some(CHAT_SERVER_CLOSED),
        "the fake took the vouch"
    );
    assert!(d.login.is_none(), "a login");
    assert_eq!(w.violations().rules(), vec!["csp-login-vouch-wrong"]);

    let w = world(207);
    let g = group(&w);
    let stranger = w.new_key_pair();
    let s = Setup {
        ck_secret: stranger.secret,
        ..setup(
            &w,
            &TestIdentity {
                id: ME.to_owned(),
                ck: stranger.clone(),
            },
        )
    };
    let d = connect(&w, &g, new_device(1), &s);
    assert_eq!(
        d.d2m.closed,
        Some(CHAT_SERVER_CLOSED),
        "an unknown identity"
    );
    assert_eq!(w.violations().rules(), vec!["csp-login-unknown-identity"]);
}

// --- Proof 11: the three spec/library differences, and Desktop's practice (G9) ---------------

/// login-ack's 16 bytes are libthreema's `reserved(4) || time || queued`, not 16 zeros: the
/// client reads the queue the fake holds and the fake's skewed clock.
#[test]
fn the_login_ack_carries_time_and_queue_as_libthreema_reads_it() {
    let (w, g, s) = scene(211, true);
    let skew: i64 = 21 * 60 * 1000;
    w.mediator().knobs(|k| k.clock_skew_ms = skew);
    for id in [1, 2] {
        assert!(
            w.mediator().csp_incoming(ME, PEER, id, 0, &[7; 40]),
            "queued"
        );
    }
    let d = logged_in(&w, &g, new_device(1), &s);
    assert_eq!(
        d.login.as_ref().map(|l| l.queued_messages),
        Some(2),
        "libthreema's queued-messages"
    );
    let (key, sck) = w.mediator().csp_session(d.d2m.conn()).expect("a session");
    let data = open_login_ack(&key, &sck, &d.raw[1]).expect("opens");
    assert_eq!(
        u64::from_le_bytes(data[4..12].try_into().expect("8 bytes")),
        START_MS + 21 * 60 * 1000,
        "the skewed time"
    );
    assert!(messages(&d).is_empty(), "messages before the unblock");
    w.violations().assert_empty();
}

/// TH3 (critic M4): the chat server's own `clock_skew_ms` moves the login-ack's time on top of the
/// mediator's knob, so a test can make the chat server's clock alone wrong.
#[test]
fn the_chat_servers_own_skew_adds_to_the_login_ack_only() {
    let time = |d2m_skew: i64, csp_skew: i64| {
        let (w, g, s) = scene(213, false);
        w.mediator().knobs(|k| {
            k.clock_skew_ms = d2m_skew;
            k.csp.clock_skew_ms = csp_skew;
        });
        let d = logged_in(&w, &g, new_device(1), &s);
        let (key, sck) = w.mediator().csp_session(d.d2m.conn()).expect("a session");
        let data = open_login_ack(&key, &sck, &d.raw[1]).expect("opens");
        (
            u64::from_le_bytes(data[4..12].try_into().expect("8 bytes")),
            w,
        )
    };
    let (alone, w1) = time(0, -21 * 60 * 1000);
    assert_eq!(
        alone,
        START_MS - 21 * 60 * 1000,
        "the chat server's skew alone"
    );
    w1.violations().assert_empty();
    let (both, w2) = time(60_000, 5_000);
    assert_eq!(both, START_MS + 65_000, "both skews add");
    w2.violations().assert_empty();
}

/// Extension 0x02 is libthreema's feature MASK `0b11` (metadata box + echo requests): read as the
/// spec's version byte (0 or 1) it would be unknown and closed; the fake takes it.
#[test]
fn extension_two_is_read_as_libthreemas_bit_mask() {
    let (w, g, s) = scene(212, false);
    let d = logged_in(&w, &g, new_device(1), &s);
    let info = w.mediator().csp_login(d.d2m.conn()).expect("the login");
    assert_eq!(info.features, Some(0b11), "libthreema's mask");
    assert!(
        !names(&d).contains(&"close-error"),
        "the mask was read as a version"
    );
    w.violations().assert_empty();
}

/// Incoming messages go out in the metadata-box form (0x02) only, which libthreema decodes:
/// sender, message id and flags at their offsets, metadata-length 0.
#[test]
fn incoming_messages_go_out_in_the_metadata_box_form() {
    let (w, g, s) = scene(213, true);
    let mut d = logged_in(&w, &g, new_device(1), &s);
    send(&w, &mut d, OutgoingPayload::UnblockIncomingMessages);
    assert!(
        w.mediator()
            .csp_incoming(ME, PEER, 0x1122, flag::SEND_PUSH, &[9; 33]),
        "queued"
    );
    pump(&w, &mut d);
    let Some(IncomingPayload::MessageWithMetadataBox(m)) = d
        .payloads
        .iter()
        .find(|p| matches!(p, IncomingPayload::MessageWithMetadataBox(_)))
    else {
        panic!("no message reached the client");
    };
    assert!(m.sender_identity.to_string() == PEER, "the sender");
    assert_eq!(m.message_id, MessageId(0x1122), "the message id");
    assert_eq!(m.flags, MessageFlags(flag::SEND_PUSH), "the flags");
    assert_eq!(m.bytes.len(), 64 + 24 + 33, "the struct's length");
    assert!(m.bytes[8..16] == *ME.as_bytes(), "the receiver");
    assert_eq!(m.bytes[30..32], [0, 0], "metadata-length");
    w.violations().assert_empty();
}

/// The CSP client-hello came before the D2M ClientHello (Desktop's order; not a difference of the
/// library but Desktop's practice where the spec is silent, G9); the fake answers it only after
/// `ServerInfo`.
#[test]
fn a_csp_client_hello_before_the_d2m_handshake_is_answered_after_server_info() {
    let (w, g, s) = scene(214, false);
    let d = logged_in(&w, &g, new_device(1), &s);
    let conn = d.d2m.conn().0;
    let lines = w.trace().lines();
    let at = |needle: &str| {
        lines
            .iter()
            .position(|l| l.split(' ').nth(1) == Some(&conn.to_string()) && l.contains(needle))
            .expect("a trace line")
    };
    assert!(
        at(" in proxy") < at(" in client-hello"),
        "the test did not send the CSP hello first"
    );
    assert!(
        at(" out server-info") < at(" out proxy"),
        "the server-hello went out before the D2M handshake ended"
    );
    w.violations().assert_empty();
}

// --- Proof 12 (second half): chunked Proxy payloads ------------------------------------------

/// The fake chunks what it sends at random boundaries and the client splits its hello: both
/// sides reassemble; the login and an echo go through.
#[test]
fn chunked_proxy_payloads_reassemble_both_ways() {
    let (w, g, mut s) = scene(221, false);
    w.mediator().knobs(|k| k.csp.chunk_proxy = true);
    s.split_hello = true;
    let mut d = logged_in(&w, &g, new_device(1), &s);
    assert!(d.raw.len() > 2, "the fake did not chunk");
    send(
        &w,
        &mut d,
        OutgoingPayload::EchoRequest(EchoPayload(vec![3; 100])),
    );
    assert_eq!(names(&d), vec!["echo-response"], "the echo");
    w.violations().assert_empty();
}

// --- Proof 13: the payloads -------------------------------------------------------------------

/// Echo both ways: the client's request is answered with its data; the server's request reaches
/// the client and its response is recorded.
#[test]
fn echo_goes_both_ways() {
    let (w, g, s) = scene(231, false);
    let mut d = logged_in(&w, &g, new_device(1), &s);
    send(
        &w,
        &mut d,
        OutgoingPayload::EchoRequest(EchoPayload(vec![1, 2, 3])),
    );
    let Some(IncomingPayload::EchoResponse(r)) = d.payloads.first() else {
        panic!("no echo response");
    };
    assert_eq!(r.0, vec![1, 2, 3], "the echo's data");
    w.mediator().csp_echo_request(d.d2m.conn(), &[4, 5]);
    pump(&w, &mut d);
    let Some(IncomingPayload::EchoRequest(q)) = d.payloads.last() else {
        panic!("no echo request");
    };
    let data = q.0.clone();
    send(&w, &mut d, OutgoingPayload::EchoResponse(EchoPayload(data)));
    assert_eq!(w.mediator().csp_echo_responses(), vec![vec![4, 5]]);
    w.violations().assert_empty();
}

/// The unblock gate: what was queued before comes only after the leader's unblock, then
/// `queue-send-complete`; an unblock outside leader mode or from a follower is a violation and
/// releases nothing.
#[test]
fn the_unblock_gate_holds_messages_until_the_leader_unblocks() {
    let (w, g, s) = scene(232, true);
    assert!(
        w.mediator().csp_incoming(ME, PEER, 5, 0, &[1; 20]),
        "queued"
    );
    let mut d = logged_in(&w, &g, new_device(1), &s);
    assert!(
        w.mediator().is_leader(d.d2m.conn()),
        "the first device leads"
    );
    assert!(messages(&d).is_empty(), "a message before the unblock");
    send(&w, &mut d, OutgoingPayload::UnblockIncomingMessages);
    assert_eq!(names(&d), vec!["message", "queue-send-complete"]);
    assert!(
        messages(&d) == vec![(PEER.to_owned(), 5)],
        "the message after the unblock"
    );
    w.violations().assert_empty();

    // A follower (the second device of the group) may not unblock.
    let mut follower = logged_in(&w, &g, new_device(2), &s);
    send(&w, &mut follower, OutgoingPayload::UnblockIncomingMessages);
    assert!(messages(&follower).is_empty(), "the follower got a message");
    assert_eq!(w.violations().rules(), vec!["csp-unblock-from-a-follower"]);

    // Outside leader mode (TH3-TH5) nobody may.
    let (w, g, s) = scene(233, false);
    assert!(
        w.mediator().csp_incoming(ME, PEER, 6, 0, &[1; 20]),
        "queued"
    );
    let mut d = logged_in(&w, &g, new_device(1), &s);
    send(&w, &mut d, OutgoingPayload::UnblockIncomingMessages);
    assert!(messages(&d).is_empty(), "released outside leader mode");
    assert_eq!(
        w.violations().rules(),
        vec!["csp-unblock-outside-leader-mode"]
    );
}

/// An incoming message stays queued until acked: a reconnect without the ack gets it again (and
/// login-ack counts it), an acked one does not come again.
#[test]
fn an_incoming_message_is_redelivered_until_acked() {
    let (w, g, s) = scene(234, true);
    let mut d = logged_in(&w, &g, new_device(1), &s);
    send(&w, &mut d, OutgoingPayload::UnblockIncomingMessages);
    for id in [10, 11] {
        assert!(
            w.mediator().csp_incoming(ME, PEER, id, 0, &[2; 24]),
            "queued"
        );
    }
    pump(&w, &mut d);
    assert!(
        messages(&d) == vec![(PEER.to_owned(), 10), (PEER.to_owned(), 11)],
        "the two messages after the unblock"
    );
    ack(&w, &mut d, PEER, 10);
    assert_eq!(w.mediator().csp_queue_len(ME), 1, "the ack");
    w.mediator().client_close(d.d2m.conn(), 1000);

    let mut d = logged_in(&w, &g, existing(1), &s);
    assert_eq!(d.login.as_ref().map(|l| l.queued_messages), Some(1));
    send(&w, &mut d, OutgoingPayload::UnblockIncomingMessages);
    assert_eq!(names(&d), vec!["message", "queue-send-complete"]);
    assert!(
        messages(&d) == vec![(PEER.to_owned(), 11)],
        "the redelivery of the one that was not acked"
    );
    ack(&w, &mut d, PEER, 11);
    assert_eq!(w.mediator().csp_queue_len(ME), 0);
    // An ack for what the server did not send on this connection is recorded.
    ack(&w, &mut d, PEER, 99);
    assert_eq!(
        w.violations().rules(),
        vec!["csp-ack-for-a-message-not-sent"]
    );
}

/// The client's own struct for an outgoing message (sender, receiver, id, created-at, flags,
/// reserved, metadata-length, nickname, nonce, box).
fn outgoing_struct(sender: &str, receiver: &str, message_id: u64, flags: u8) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(sender.as_bytes());
    out.extend_from_slice(receiver.as_bytes());
    out.extend_from_slice(&message_id.to_le_bytes());
    out.extend_from_slice(&1_760_000_000u32.to_le_bytes());
    out.push(flags);
    out.push(0);
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&[0; 32]);
    out.extend_from_slice(&[8; 24]);
    out.extend_from_slice(&[6; 40]);
    out
}

fn outgoing(sender: &str, receiver: &str, message_id: u64, flags: u8) -> OutgoingPayload {
    outgoing_from(
        sender,
        message_id,
        flags,
        outgoing_struct(sender, receiver, message_id, flags),
    )
}

/// An outgoing message with the struct's bytes as the test made them.
fn outgoing_from(sender: &str, message_id: u64, flags: u8, bytes: Vec<u8>) -> OutgoingPayload {
    OutgoingPayload::MessageWithMetadataBox(MessageWithMetadataBox {
        sender_identity: ThreemaId::try_from(sender).expect("an ID"),
        message_id: MessageId(message_id),
        flags: MessageFlags(flags),
        bytes,
    })
}

/// An outgoing message is acked with the receiver and the id and kept for the peer; with "no
/// server ack" it is kept and not acked; a sender that is not the login identity is refused.
#[test]
fn an_outgoing_message_is_acked_and_kept_for_the_peer() {
    let (w, g, s) = scene(235, false);
    let mut d = logged_in(&w, &g, new_device(1), &s);
    send(&w, &mut d, outgoing(ME, PEER, 0x77, 0));
    let Some(IncomingPayload::MessageAck(a)) = d.payloads.first() else {
        panic!("no message ack");
    };
    assert!(
        a.sender_identity.to_string() == PEER,
        "the ack names the receiver"
    );
    assert_eq!(a.message_id, MessageId(0x77));
    send(&w, &mut d, outgoing(ME, PEER, 0x78, flag::NO_SERVER_ACK));
    assert_eq!(d.payloads.len(), 1, "a message without ack was acked");
    let sent = w.mediator().csp_sent();
    assert_eq!(
        sent.iter()
            .map(|m| (m.message_id, m.flags))
            .collect::<Vec<_>>(),
        vec![(0x77, 0), (0x78, flag::NO_SERVER_ACK)]
    );
    assert!(
        sent[0].receiver == *PEER.as_bytes() && sent[0].bytes == outgoing_struct(ME, PEER, 0x77, 0),
        "the kept bytes"
    );
    w.violations().assert_empty();
    send(&w, &mut d, outgoing(PEER, ME, 0x79, 0));
    assert_eq!(d.d2m.closed, Some(CHAT_SERVER_CLOSED), "a forged sender");
    assert_eq!(
        w.violations().rules(),
        vec!["csp-outgoing-sender-is-not-the-login-identity"]
    );
}

/// Flag 0x04: delivered once, not kept for an ack; flag 0x02: dropped while nobody is unblocked,
/// delivered when somebody is; 0x20: gone after 60 s on the virtual clock.
#[test]
fn the_no_ack_no_queuing_and_short_lived_flags() {
    let (w, g, s) = scene(236, true);
    assert!(
        !w.mediator()
            .csp_incoming(ME, PEER, 1, flag::NO_SERVER_QUEUING, &[0; 16]),
        "a no-queuing message was queued while nobody was unblocked"
    );
    assert!(
        w.mediator()
            .csp_incoming(ME, PEER, 2, flag::SHORT_LIVED, &[0; 16]),
        "queued"
    );
    w.mediator().advance(Duration::from_secs(60));
    assert_eq!(w.mediator().csp_queue_len(ME), 0, "the short-lived one");
    let mut d = logged_in(&w, &g, new_device(1), &s);
    send(&w, &mut d, OutgoingPayload::UnblockIncomingMessages);
    assert!(messages(&d).is_empty(), "a dropped message came");
    assert!(
        w.mediator()
            .csp_incoming(ME, PEER, 3, flag::NO_SERVER_ACK, &[0; 16]),
        "queued"
    );
    assert!(
        w.mediator()
            .csp_incoming(ME, PEER, 4, flag::NO_SERVER_QUEUING, &[0; 16]),
        "delivered"
    );
    pump(&w, &mut d);
    assert!(
        messages(&d) == vec![(PEER.to_owned(), 3), (PEER.to_owned(), 4)],
        "the messages that were deliverable"
    );
    assert_eq!(
        w.mediator().csp_queue_len(ME),
        1,
        "only the one that wants an ack"
    );
    w.violations().assert_empty();
}

/// `close-error{can-reconnect: 0}` and then a close code that alone would reconnect (4000): the
/// client sees both, and `close.rs` lets the close-error decide.
#[test]
fn a_close_error_then_a_contradicting_code_the_close_error_wins() {
    let (w, g, s) = scene(237, false);
    let mut d = logged_in(&w, &g, new_device(1), &s);
    w.mediator()
        .csp_close_error(d.d2m.conn(), false, CHAT_SERVER_CLOSED);
    pump(&w, &mut d);
    let Some(IncomingPayload::CloseError(e)) = d.payloads.last() else {
        panic!("no close-error");
    };
    assert!(!e.can_reconnect, "can-reconnect");
    let code = d.d2m.closed.expect("the close");
    assert_eq!(code, 4000);
    assert_eq!(verdict(code, None), Verdict::Reconnect { warn: false });
    assert_eq!(
        verdict(
            code,
            Some(CloseError {
                can_reconnect: e.can_reconnect
            })
        ),
        Verdict::Stop(Stop::ChatServerRefused),
        "the close-error did not win"
    );
    w.violations().assert_empty();
}

/// An alert reaches the client; push-token calls are only recorded (Kourier sends none: the
/// recorder works, so TH3's "none" means something).
#[test]
fn an_alert_reaches_the_client_and_push_token_calls_are_recorded() {
    let (w, g, s) = scene(238, false);
    let mut d = logged_in(&w, &g, new_device(1), &s);
    assert!(
        w.mediator().csp_push_calls().is_empty(),
        "a push call at login"
    );
    w.mediator().csp_alert(d.d2m.conn());
    pump(&w, &mut d);
    assert_eq!(names(&d), vec!["alert"]);
    send(
        &w,
        &mut d,
        OutgoingPayload::SetPushNotificationToken(PushNotificationToken::Clear),
    );
    send(
        &w,
        &mut d,
        OutgoingPayload::DeletePushNotificationToken(DeletePushNotificationToken::All),
    );
    assert_eq!(
        w.mediator().csp_push_calls(),
        vec![csp_kind::SET_PUSH_TOKEN, csp_kind::DELETE_PUSH_TOKEN]
    );
    assert!(d.logged_in(), "the connection");
    w.violations().assert_empty();
}

/// The device cookie of the last connection: another one (or none while one is stored) sets the
/// flag and every login gets the indication until the client clears it.
#[test]
fn a_device_cookie_change_is_indicated_until_cleared() {
    let (w, g, mut s) = scene(239, false);
    let login = |s: &Setup, first: bool| {
        let o = if first { new_device(1) } else { existing(1) };
        let d = logged_in(&w, &g, o, s);
        let got = names(&d);
        (d, got)
    };
    let (d, got) = login(&s, true);
    assert!(got.is_empty(), "an indication at the first login");
    w.mediator().client_close(d.d2m.conn(), 1000);
    s.cookie = Some([0x6d; 16]);
    let (d, got) = login(&s, false);
    assert_eq!(got, vec!["device-cookie-change-indication"], "a new cookie");
    w.mediator().client_close(d.d2m.conn(), 1000);
    let (mut d, got) = login(&s, false);
    assert_eq!(
        got,
        vec!["device-cookie-change-indication"],
        "until cleared"
    );
    send(
        &w,
        &mut d,
        OutgoingPayload::ClearDeviceCookieChangeIndiciation,
    );
    w.mediator().client_close(d.d2m.conn(), 1000);
    let (d, got) = login(&s, false);
    assert!(got.is_empty(), "after the clear");
    w.mediator().client_close(d.d2m.conn(), 1000);
    s.cookie = None;
    let (_, got) = login(&s, false);
    assert_eq!(got, vec!["device-cookie-change-indication"], "no cookie");
    w.violations().assert_empty();
}

/// `set-connection-idle-timeout` is seconds (the spec; libthreema's doc says minutes) and sets the
/// CHAT SERVER's idle timer of this connection (G2), not the mediator's: the leg ends at the
/// timeout and the mediator closes the connection with 4000 (the code is [U: real server]).
#[test]
fn set_connection_idle_timeout_sets_the_chat_servers_idle_close_in_seconds() {
    let (w, g, s) = scene(240, false);
    let mut d = logged_in(&w, &g, new_device(1), &s);
    let conn = d.d2m.conn();
    assert_eq!(
        w.mediator().csp_idle_timeout(conn),
        Some(DEFAULT_CSP_IDLE_TIMEOUT),
        "the chat server's default"
    );
    send(
        &w,
        &mut d,
        OutgoingPayload::SetConnectionIdleTimeout(ConnectionIdleTimeout(30)),
    );
    assert_eq!(w.mediator().csp_idle_requests(), vec![30]);
    assert_eq!(
        w.mediator().csp_idle_timeout(conn),
        Some(Duration::from_secs(30)),
        "seconds, not minutes"
    );
    assert_eq!(
        w.mediator().idle_timeout(conn),
        Some(DEFAULT_IDLE_TIMEOUT),
        "the mediator's own timer is another one"
    );
    w.mediator()
        .advance(Duration::from_secs(30) - Duration::from_millis(1));
    pump(&w, &mut d);
    assert_eq!(d.d2m.closed, None, "closed before the timeout");
    w.mediator().advance(Duration::from_millis(1));
    pump(&w, &mut d);
    assert_eq!(
        d.d2m.closed,
        Some(CHAT_SERVER_CLOSED),
        "the chat server's idle close"
    );
    w.violations().assert_empty();
}

/// G2 (the spec review's MEDIUM-2): the chat server sees only the CSP bytes, so D2M frames do not
/// keep its idle timer alive. The leader reflects every 20 s and sends no CSP frame: the CSP leg
/// ends at 30 s (checked at 40 s), the mediator's own timer (5 minutes, reset by the reflects)
/// being far from over.
#[test]
fn d2m_frames_alone_do_not_keep_the_chat_servers_idle_timer_alive() {
    let (w, g, s) = scene(243, false);
    let mut d = logged_in(&w, &g, new_device(1), &s);
    send(
        &w,
        &mut d,
        OutgoingPayload::SetConnectionIdleTimeout(ConnectionIdleTimeout(30)),
    );
    for (n, id) in [1_u32, 2].into_iter().enumerate() {
        w.mediator().advance(Duration::from_secs(20));
        if n == 0 {
            reflect_on_d2m(&w, &mut d, id);
            pump(&w, &mut d);
            assert_eq!(d.d2m.closed, None, "closed at 20 s, before the timeout");
        } else {
            pump(&w, &mut d);
        }
    }
    assert_eq!(
        d.d2m.closed,
        Some(CHAT_SERVER_CLOSED),
        "D2M frames kept the chat server's timer alive"
    );
    w.violations().assert_empty();
}

/// The other half of the same rule, and what TH3 must do: CSP echo requests every 20 s keep the CSP
/// leg alive for two minutes against a 30 s timeout; the silence after them ends it.
#[test]
fn csp_echo_requests_keep_the_chat_servers_idle_timer_alive() {
    let (w, g, s) = scene(246, false);
    let mut d = logged_in(&w, &g, new_device(1), &s);
    send(
        &w,
        &mut d,
        OutgoingPayload::SetConnectionIdleTimeout(ConnectionIdleTimeout(30)),
    );
    for n in 0..6u8 {
        w.mediator().advance(Duration::from_secs(20));
        send(
            &w,
            &mut d,
            OutgoingPayload::EchoRequest(EchoPayload(vec![n])),
        );
        assert!(d.logged_in(), "the echo request {n} did not keep it alive");
    }
    w.mediator().advance(Duration::from_secs(30));
    pump(&w, &mut d);
    assert_eq!(
        d.d2m.closed,
        Some(CHAT_SERVER_CLOSED),
        "the silence after the echoes"
    );
    w.violations().assert_empty();
}

/// A reflect on the D2M leg (a D2M frame that is no CSP).
fn reflect_on_d2m(w: &MockWorld, d: &mut Device, reflect_id: u32) {
    super::d2m::send(
        w,
        &mut d.d2m,
        D2mOutgoing::Reflect(Reflect {
            flags: ReflectFlags(0),
            reflect_id,
            envelope: vec![1; 40],
        }),
    );
}

/// G10: a short-lived (0x20) message is queued for 60 seconds, but the one that WAS delivered on
/// this connection may still be acked later: only a second ack, or an ack for a message that wants
/// none (0x04), is the client's fault.
#[test]
fn an_ack_after_the_60_seconds_of_a_short_lived_message_is_no_client_fault() {
    let (w, g, s) = scene(244, true);
    let mut d = logged_in(&w, &g, new_device(1), &s);
    send(&w, &mut d, OutgoingPayload::UnblockIncomingMessages);
    assert!(
        w.mediator()
            .csp_incoming(ME, PEER, 1, flag::SHORT_LIVED, &[0; 16]),
        "queued"
    );
    pump(&w, &mut d);
    assert!(
        messages(&d) == vec![(PEER.to_owned(), 1)],
        "the short-lived message did not arrive"
    );
    w.mediator().advance(Duration::from_secs(61));
    pump(&w, &mut d);
    assert_eq!(w.mediator().csp_queue_len(ME), 0, "its queue time ran out");
    ack(&w, &mut d, PEER, 1);
    w.violations().assert_empty();
    // A second ack for it, and an ack for a message that expects none, are not fine.
    ack(&w, &mut d, PEER, 1);
    assert!(
        w.mediator()
            .csp_incoming(ME, PEER, 2, flag::NO_SERVER_ACK, &[0; 16]),
        "queued"
    );
    pump(&w, &mut d);
    ack(&w, &mut d, PEER, 2);
    assert_eq!(
        w.violations().rules(),
        vec![
            "csp-ack-for-a-message-not-sent",
            "csp-ack-for-a-message-not-sent"
        ]
    );
}

// --- G7: what the fake no longer forgives ------------------------------------------------------

/// A hand-made CSP client on the fake's own crypto: it logs in with exactly the extension bytes the
/// test gives (libthreema always sends extension 0x02, so these logins cannot come from it).
struct Raw {
    chunks: Vec<Vec<u8>>,
}

/// The Proxy payloads the fake sent so far (the raw bytes, in order).
fn proxy_chunks(c: &Client) -> Vec<Vec<u8>> {
    c.payloads
        .iter()
        .filter_map(|p| match p {
            D2mIncoming::Proxy(b) => Some(b.clone()),
            _ => None,
        })
        .collect()
}

/// Logs the identity `me` in through `c` (a D2M client past its handshake) with `extensions` as the
/// login's extension bytes (plain; the box and its length are made here).
fn raw_login(w: &MockWorld, c: &mut Client, me: &TestIdentity, extensions: &[u8]) -> Raw {
    let tck = w.new_key_pair();
    let cck: [u8; 16] = w.random();
    let mut hello = tck.public.to_vec();
    hello.extend_from_slice(&cck);
    super::d2m::send(w, c, D2mOutgoing::Proxy(hello));
    let server_hello = proxy_chunks(c).concat();
    assert_eq!(server_hello.len(), 80, "the server-hello");
    let sck: [u8; 16] = server_hello[..16].try_into().expect("16 bytes");
    let hello_key = hsalsa_shared(&tck.secret, &w.server_key().public).expect("contributory");
    let opened =
        open(&hello_key, &csp_nonce(&sck, 1), &server_hello[16..]).expect("the server-hello opens");
    let tsk_public: [u8; 32] = opened[..32].try_into().expect("32 bytes");
    assert!(opened[32..] == cck, "the repeated CCK");
    let key = hsalsa_shared(&tck.secret, &tsk_public).expect("contributory");
    let vouch = vouch_from_secrets(
        &hsalsa_shared(&me.ck.secret, &w.server_key().public).expect("contributory"),
        &hsalsa_shared(&me.ck.secret, &tsk_public).expect("contributory"),
        &sck,
        &tck.public,
    );
    let mut data = Vec::with_capacity(128);
    data.extend_from_slice(me.id.as_bytes());
    data.extend_from_slice(b"threema-clever-extension-field");
    let boxed_extensions = u16::try_from(extensions.len() + 16).expect("short extensions");
    data.extend_from_slice(&boxed_extensions.to_le_bytes());
    data.extend_from_slice(&sck);
    data.extend_from_slice(&[0; 24]);
    data.extend_from_slice(&vouch);
    data.extend_from_slice(&[0; 16]);
    let mut client = CspNonces::new(cck);
    let mut login = client.seal(&key, &data).expect("CSN 1");
    login.extend_from_slice(&client.seal(&key, extensions).expect("CSN 2"));
    super::d2m::send(w, c, D2mOutgoing::Proxy(login));
    Raw {
        chunks: proxy_chunks(c),
    }
}

/// Extension 0x02: the payload version / feature byte.
fn features_extension(features: u8) -> Vec<u8> {
    vec![0x02, 1, 0, features]
}

/// A client-info extension (0x00) with a short text.
fn client_info_extension() -> Vec<u8> {
    vec![0x00, 3, 0, b'r', b'a', b'w']
}

/// G7: a login WITHOUT extension 0x02 selects the spec's legacy form (`csp.struct.yml:355-356`),
/// which the fake does not serve; a feature mask without the metadata-box bit is the same: both are
/// recorded and end with a `close-error`, then the chat server's close. [I]
#[test]
fn a_login_without_the_metadata_box_form_is_recorded_and_closed() {
    for (seed, extensions, rule) in [
        (
            247,
            client_info_extension(),
            "csp-no-payload-version-extension",
        ),
        (248, features_extension(0b10), "csp-no-metadata-box-support"),
    ] {
        let w = world(seed);
        let me = w.add_identity(ME);
        let g = group(&w);
        let mut c = super::d2m::online(&w, &g, new_device(1));
        raw_login(&w, &mut c, &me, &extensions);
        assert_eq!(
            c.closed,
            Some(CHAT_SERVER_CLOSED),
            "the chat server's close"
        );
        assert_eq!(w.violations().rules(), vec![rule]);
    }
}

/// G7: an echo request for a client that did not declare echo support (feature bit 1) is not sent;
/// the call is recorded (a bug of the test, [I]).
#[test]
fn an_echo_request_for_a_client_without_echo_support_is_not_sent() {
    let w = world(249);
    let me = w.add_identity(ME);
    let g = group(&w);
    let mut c = super::d2m::online(&w, &g, new_device(1));
    let raw = raw_login(&w, &mut c, &me, &features_extension(0b01));
    assert_eq!(raw.chunks.len(), 2, "server-hello and login-ack only");
    w.mediator().csp_echo_request(c.conn(), &[1]);
    super::d2m::pump(w.mediator(), &mut c);
    assert_eq!(
        proxy_chunks(&c).len(),
        2,
        "an echo request went to a client without the feature"
    );
    assert_eq!(
        w.violations().rules(),
        vec!["csp-echo-request-without-client-support"]
    );
}

/// G7: an outgoing message whose reserved byte is not zero is recorded (the fake goes on); one whose
/// metadata-length runs over the payload is refused like a malformed message. [I]
#[test]
fn an_outgoing_message_with_a_wrong_reserved_byte_or_metadata_length_is_recorded() {
    let (w, g, s) = scene(250, false);
    let mut d = logged_in(&w, &g, new_device(1), &s);
    let mut bytes = outgoing_struct(ME, PEER, 0x31, 0);
    bytes[29] = 1;
    send(&w, &mut d, outgoing_from(ME, 0x31, 0, bytes));
    assert!(d.logged_in(), "a reserved byte closed the connection");
    assert_eq!(w.mediator().csp_sent().len(), 1, "the message was kept");
    assert_eq!(
        w.violations().rules(),
        vec!["csp-outgoing-reserved-not-zero"]
    );
    let mut bytes = outgoing_struct(ME, PEER, 0x32, 0);
    bytes[30..32].copy_from_slice(&u16::MAX.to_le_bytes());
    send(&w, &mut d, outgoing_from(ME, 0x32, 0, bytes));
    assert_eq!(d.d2m.closed, Some(CHAT_SERVER_CLOSED), "a metadata overrun");
    assert_eq!(
        w.violations().rules(),
        vec![
            "csp-outgoing-reserved-not-zero",
            "csp-outgoing-metadata-length-overruns"
        ]
    );
}

/// The server's sequence numbers count on from the login-ack's 2 without a gap: three echoes
/// open; a frame the test seals with one number skipped does not (the client counts strictly).
#[test]
fn the_servers_sequence_numbers_count_on_without_a_gap() {
    let (w, g, s) = scene(241, false);
    let mut d = logged_in(&w, &g, new_device(1), &s);
    for n in 0..3u8 {
        send(
            &w,
            &mut d,
            OutgoingPayload::EchoRequest(EchoPayload(vec![n])),
        );
    }
    assert_eq!(
        names(&d),
        vec!["echo-response", "echo-response", "echo-response"],
        "the server's frames"
    );
    assert!(!d.csp_failed, "a frame did not open");
    // SSN 1 server-hello, 2 login-ack, 3-5 the echoes: 6 is next; 7 skips one.
    let (key, sck) = w.mediator().csp_session(d.d2m.conn()).expect("a session");
    let skipped = seal_frame(&key, &sck, 7, &[csp_kind::ECHO_REQUEST, 0, 0, 0, 1]);
    w.mediator()
        .send_raw(d.d2m.conn(), datagram(kind::PROXY, &skipped));
    pump(&w, &mut d);
    assert!(d.csp_failed, "the client took a skipped sequence number");
    w.violations().assert_empty();
}

// --- Proof 14 (CSP half): the frame limit, both sides ----------------------------------------

/// 8,192 bytes per CSP frame, the 2-byte length counted (box 8,190): taken and answered; 8,193:
/// refused, the connection dropped (4000) and recorded. The client's own encoder makes the frames.
#[test]
fn a_csp_frame_of_8192_bytes_is_taken_and_one_of_8193_refused() {
    let (w, g, s) = scene(242, false);
    let mut d = logged_in(&w, &g, new_device(1), &s);
    // frame = 2 + box; box = container (4 + data) + tag (16).
    let data = MAX_FRAME - 2 - 4 - 16;
    let frame = d
        .csp
        .create_payload(&OutgoingPayload::EchoRequest(EchoPayload(vec![1; data])))
        .expect("encode")
        .outgoing_frame
        .expect("a frame")
        .0;
    assert_eq!(frame.len(), 8_192, "the client's frame");
    proxy(&w, &mut d, frame);
    pump(&w, &mut d);
    let Some(IncomingPayload::EchoResponse(r)) = d.payloads.last() else {
        panic!("the frame at the limit was not answered");
    };
    assert_eq!(r.0.len(), data, "the answer at the limit");
    w.violations().assert_empty();
    let frame = d
        .csp
        .create_payload(&OutgoingPayload::EchoRequest(EchoPayload(vec![
            1;
            data + 1
        ])))
        .expect("encode")
        .outgoing_frame
        .expect("a frame")
        .0;
    assert_eq!(frame.len(), 8_193, "the client's frame");
    proxy(&w, &mut d, frame);
    pump(&w, &mut d);
    assert_eq!(
        d.d2m.closed,
        Some(CHAT_SERVER_CLOSED),
        "a frame over the limit"
    );
    assert_eq!(w.violations().rules(), vec!["csp-frame-over-8192-bytes"]);
}

/// TH3 (critic M4): `withhold_echo` takes the client's echo requests and answers none; the fake
/// counts every echo request that reached the chat server, answered or not.
#[test]
fn the_chat_server_counts_echo_requests_and_can_withhold_the_answer() {
    let (w, g, s) = scene(247, false);
    let mut d = logged_in(&w, &g, new_device(1), &s);
    let echo = |n: u8| OutgoingPayload::EchoRequest(EchoPayload(vec![n]));
    send(&w, &mut d, echo(1));
    assert_eq!(names(&d), vec!["echo-response"], "answered by default");
    assert_eq!(w.mediator().csp_echo_requests(), 1);
    w.mediator().knobs(|k| k.csp.withhold_echo = true);
    send(&w, &mut d, echo(2));
    send(&w, &mut d, echo(3));
    assert_eq!(
        names(&d),
        vec!["echo-response"],
        "a withheld echo was answered"
    );
    assert_eq!(
        w.mediator().csp_echo_requests(),
        3,
        "withheld ones count too"
    );
    assert!(d.logged_in(), "withholding closed the connection");
    w.mediator().knobs(|k| k.csp.withhold_echo = false);
    send(&w, &mut d, echo(4));
    assert_eq!(names(&d), vec!["echo-response", "echo-response"]);
    assert_eq!(w.mediator().csp_echo_requests(), 4);
    w.violations().assert_empty();
}

/// TH3 (critic M4): a black-holed connection takes the client's frames and drops them, sends
/// nothing (not even a server's echo request), and neither idle timer closes it, also after an
/// hour; a client close still ends it, and a new connection of the device works as before.
#[test]
fn a_black_holed_connection_drops_everything_and_never_times_out() {
    let (w, g, s) = scene(248, false);
    let mut d = logged_in(&w, &g, new_device(1), &s);
    let conn = d.d2m.conn();
    w.mediator().black_hole(conn);
    send(
        &w,
        &mut d,
        OutgoingPayload::EchoRequest(EchoPayload(vec![1])),
    );
    assert!(d.payloads.is_empty(), "a black hole answered");
    assert_eq!(
        w.mediator().csp_echo_requests(),
        0,
        "the frame reached the chat server"
    );
    w.mediator().csp_echo_request(conn, &[2]);
    w.mediator().advance(Duration::from_secs(3_600));
    pump(&w, &mut d);
    assert!(d.payloads.is_empty(), "a black hole sent something");
    assert_eq!(d.d2m.closed, None, "a timer closed the black hole");
    assert_eq!(w.mediator().closed(conn), None);
    w.mediator().client_close(conn, 1000);
    assert_eq!(w.mediator().closed(conn), Some(1000), "the client's close");
    let mut again = logged_in(&w, &g, existing(1), &s);
    send(
        &w,
        &mut again,
        OutgoingPayload::EchoRequest(EchoPayload(vec![3])),
    );
    assert_eq!(names(&again), vec!["echo-response"], "the next connection");
    w.violations().assert_empty();
}

// --- TH6a S3: the chat server's side of sending ------------------------------------------------

const PEER2: &str = "PEER0002";

/// The client's payload for a message of ME to `to` that the test sealed itself (`peer.rs`'s `Wire`):
/// the same arguments give the same bytes.
fn boxed(w: &MockWorld, to: &str, id: u64, nonce: u8) -> OutgoingPayload {
    let me = w.identity(ME).expect("ME is in the table");
    let to = w.identity(to).expect("the receiver is in the table");
    let wire = Wire::new(&me, &to, id, nonce);
    outgoing_from(ME, id, wire.flags, wire.sent().bytes)
}

/// The (receiver, id) of the `message-ack`s a device got, in order.
fn acks(d: &Device) -> Vec<(String, u64)> {
    d.payloads
        .iter()
        .filter_map(|p| match p {
            IncomingPayload::MessageAck(a) => Some((a.sender_identity.to_string(), a.message_id.0)),
            _ => None,
        })
        .collect()
}

fn ids(opened: &[Opened]) -> Vec<u64> {
    opened.iter().map(|o| o.message_id).collect()
}

fn kept_ids(w: &MockWorld) -> Vec<u64> {
    w.mediator()
        .csp_sent()
        .iter()
        .map(|m| m.message_id)
        .collect()
}

/// The peer drains what the chat server kept for its ID, once, in order, and leaves the others'
/// messages to their peers; every message opens and the connection it came in on is recorded.
#[test]
fn the_peer_drains_what_the_chat_server_kept_once_and_only_its_own() {
    let (w, g, s) = scene(260, false);
    w.add_identity(PEER2);
    let mut d = logged_in(&w, &g, new_device(1), &s);
    let (mut one, mut two) = (w.peer(PEER), w.peer(PEER2));
    for (id, to, nonce) in [(0x61, PEER, 1), (0x62, PEER2, 2), (0x63, PEER, 3)] {
        send(&w, &mut d, boxed(&w, to, id, nonce));
    }
    let got = one.drain();
    assert_eq!(ids(&got), vec![0x61, 0x63], "its own, in order");
    assert!(
        got.iter().all(|o| o.conn == d.d2m.conn().0),
        "the connection it came in on"
    );
    assert!(one.drain().is_empty(), "a second drain");
    send(&w, &mut d, boxed(&w, PEER, 0x64, 4));
    assert_eq!(ids(&one.drain()), vec![0x64], "what came after");
    assert_eq!(ids(&two.drain()), vec![0x62], "the other peer's own");
    assert_eq!(one.opened().len(), 3);
    assert_eq!(
        acks(&d),
        vec![
            (PEER.to_owned(), 0x61),
            (PEER2.to_owned(), 0x62),
            (PEER.to_owned(), 0x63),
            (PEER.to_owned(), 0x64)
        ]
    );
    w.violations().assert_empty();
}

/// With no knob the chat server has no memory of ids or nonces: the very same bytes are kept and
/// acked a second time (as a reconnect's resend would be), and the peer reads the copy as a
/// discard by its nonce.
#[test]
fn the_same_bytes_again_are_acked_kept_and_read_by_the_peer_as_a_nonce_discard() {
    let (w, g, s) = scene(261, false);
    let mut d = logged_in(&w, &g, new_device(1), &s);
    let mut peer = w.peer(PEER);
    send(&w, &mut d, boxed(&w, PEER, 0x71, 5));
    send(&w, &mut d, boxed(&w, PEER, 0x71, 5));
    assert_eq!(
        acks(&d),
        vec![(PEER.to_owned(), 0x71), (PEER.to_owned(), 0x71)],
        "both acked"
    );
    assert_eq!(kept_ids(&w), vec![0x71, 0x71], "both kept");
    let got = peer.drain();
    assert_eq!(
        got.iter().map(|o| o.discard).collect::<Vec<_>>(),
        vec![None, Some(Discard::Nonce)]
    );
    assert_eq!(got[0].bytes, got[1].bytes, "equal bytes");
    assert!(w.mediator().csp_repeats_dropped().is_empty());
    w.violations().assert_empty();
}

/// The server's rule on `created-at` (replaced by now when it lies ahead): the head then differs
/// from the metadata's original time and the peer takes that, because the spec keeps the original
/// in the metadata for it.
#[test]
fn a_created_at_the_server_replaced_is_no_difference_to_the_peer() {
    let (w, g, s) = scene(262, false);
    w.mediator().knobs(|k| k.csp.replace_created_at = true);
    let me = w.identity(ME).expect("me");
    let to = w.identity(PEER).expect("peer");
    let mut wire = Wire::new(&me, &to, 0x72, 6);
    wire.created_ms = START_MS + 3_600_000;
    let mut d = logged_in(&w, &g, new_device(1), &s);
    send(
        &w,
        &mut d,
        outgoing_from(ME, 0x72, wire.flags, wire.sent().bytes),
    );
    let kept = w.mediator().csp_sent();
    assert!(kept[0].replaced, "the server replaced the time");
    assert_eq!(u64::from(kept[0].created_at), START_MS / 1000, "by now");
    let got = w.peer(PEER).drain();
    assert_eq!(got.len(), 1, "opened");
    assert_eq!(got[0].created_at_ms, START_MS + 3_600_000, "the original");
    w.violations().assert_empty();
}

/// A head that differs from the metadata with nobody to blame but the client is the client's
/// breach.
#[test]
fn a_created_at_that_differs_without_the_server_is_a_breach_of_the_client() {
    let (w, g, s) = scene(263, false);
    let me = w.identity(ME).expect("me");
    let to = w.identity(PEER).expect("peer");
    let mut wire = Wire::new(&me, &to, 0x73, 7);
    wire.head_s = Some(5);
    let mut d = logged_in(&w, &g, new_device(1), &s);
    send(
        &w,
        &mut d,
        outgoing_from(ME, 0x73, wire.flags, wire.sent().bytes),
    );
    assert!(w.peer(PEER).drain().is_empty(), "the peer opened it");
    assert_eq!(w.violations().rules(), vec!["peer-created-at-differs"]);
}

/// `close_on_box_to(receiver, code)`: a box to that receiver closes the connection with the code, is
/// kept and acked by nobody and shown to no peer, only counted; another receiver is not touched;
/// lifted, the same box goes through.
#[test]
fn a_box_to_a_receiver_with_a_close_rule_closes_the_connection_and_is_kept_by_nobody() {
    let (w, g, s) = scene(264, false);
    w.add_identity(PEER2);
    w.mediator().knobs(|k| k.csp.close_on_box_to(PEER, 4003));
    let mut d = logged_in(&w, &g, new_device(1), &s);
    let conn = d.d2m.conn();
    send(&w, &mut d, boxed(&w, PEER2, 0x81, 1));
    assert_eq!(d.d2m.closed, None, "a box to another receiver");
    send(&w, &mut d, boxed(&w, PEER, 0x82, 2));
    assert_eq!(d.d2m.closed, Some(4003), "the client saw the code");
    assert_eq!(w.mediator().closed(conn), Some(4003), "the mediator's");
    assert_eq!(acks(&d), vec![(PEER2.to_owned(), 0x81)], "no ack");
    assert_eq!(kept_ids(&w), vec![0x81], "the refused box is not kept");
    assert!(w.peer(PEER).drain().is_empty(), "the peer got it");
    assert_eq!(
        w.mediator().csp_refused(),
        vec![(conn, PEER.to_owned(), 0x82)]
    );
    assert!(
        w.trace()
            .lines()
            .iter()
            .any(|l| l.ends_with("csp-outgoing-message-refused code=4003")),
        "the trace names the refusal"
    );

    // Lifted: a new login, and the very same box is taken and acked.
    w.mediator().knobs(|k| k.csp.stop_closing_on_box_to(PEER));
    let mut again = logged_in(&w, &g, existing(1), &s);
    send(&w, &mut again, boxed(&w, PEER, 0x82, 2));
    assert_eq!(again.d2m.closed, None);
    assert_eq!(acks(&again), vec![(PEER.to_owned(), 0x82)]);
    assert_eq!(kept_ids(&w), vec![0x81, 0x82]);
    assert_eq!(w.mediator().csp_refused().len(), 1, "no new refusal");
    w.violations().assert_empty();
}

/// `withhold_message_ack_of(receiver)`: the box is taken and kept for the peer, its ack is held
/// (other receivers' acks go as always) until `release_message_acks`, which sends it in order and
/// stops holding; an ack held on a connection that closed meanwhile is lost with it.
#[test]
fn a_withheld_message_ack_is_held_until_released_and_holding_stops_with_the_release() {
    let (w, g, s) = scene(265, false);
    w.add_identity(PEER2);
    w.mediator().knobs(|k| k.csp.withhold_message_ack_of(PEER));
    let mut d = logged_in(&w, &g, new_device(1), &s);
    send(&w, &mut d, boxed(&w, PEER, 0x91, 1));
    send(&w, &mut d, boxed(&w, PEER2, 0x92, 2));
    assert_eq!(acks(&d), vec![(PEER2.to_owned(), 0x92)], "only the other");
    assert_eq!(kept_ids(&w), vec![0x91, 0x92], "both kept");
    assert_eq!(ids(&w.peer(PEER).drain()), vec![0x91], "the peer has it");
    assert_eq!(w.mediator().csp_held_acks(PEER), 1);
    assert_eq!(w.mediator().csp_held_acks(PEER2), 0);

    assert_eq!(w.mediator().release_message_acks(PEER), 1);
    pump(&w, &mut d);
    assert_eq!(
        acks(&d),
        vec![(PEER2.to_owned(), 0x92), (PEER.to_owned(), 0x91)],
        "the released ack"
    );
    assert_eq!(w.mediator().csp_held_acks(PEER), 0);
    assert_eq!(w.mediator().release_message_acks(PEER), 0, "nothing twice");
    send(&w, &mut d, boxed(&w, PEER, 0x93, 3));
    assert_eq!(
        acks(&d).last(),
        Some(&(PEER.to_owned(), 0x93)),
        "holding stopped with the release"
    );

    // Held again; the connection goes; the ack goes with it.
    w.mediator().knobs(|k| k.csp.withhold_message_ack_of(PEER));
    send(&w, &mut d, boxed(&w, PEER, 0x94, 4));
    assert_eq!(w.mediator().csp_held_acks(PEER), 1);
    w.mediator().client_close(d.d2m.conn(), 1000);
    assert_eq!(
        w.mediator().release_message_acks(PEER),
        0,
        "a closed socket"
    );
    w.violations().assert_empty();
}

/// `drop_repeats`: a box whose nonce the chat server took before is dropped, not kept, not acked
/// (the client waits for the ack), counted; a fresh nonce under the same id is no repeat to it; with
/// the knob off the same bytes are kept and acked again.
#[test]
fn a_box_with_a_known_nonce_is_dropped_when_the_knob_says_so() {
    let (w, g, s) = scene(266, false);
    w.mediator().knobs(|k| k.csp.drop_repeats = true);
    let mut d = logged_in(&w, &g, new_device(1), &s);
    let conn = d.d2m.conn();
    let mut peer = w.peer(PEER);
    send(&w, &mut d, boxed(&w, PEER, 0xa1, 1));
    send(&w, &mut d, boxed(&w, PEER, 0xa1, 1));
    assert_eq!(acks(&d), vec![(PEER.to_owned(), 0xa1)], "one ack");
    assert_eq!(kept_ids(&w), vec![0xa1], "one kept");
    assert_eq!(
        w.mediator().csp_repeats_dropped(),
        vec![(conn, PEER.to_owned(), 0xa1)]
    );
    assert!(
        w.trace()
            .lines()
            .iter()
            .any(|l| l.contains(" in csp-outgoing-repeat-dropped ")),
        "the trace names the drop"
    );

    // The same id under a fresh nonce: kept and acked, the peer discards it by the id.
    send(&w, &mut d, boxed(&w, PEER, 0xa1, 2));
    assert_eq!(acks(&d).len(), 2);
    assert_eq!(
        peer.drain().iter().map(|o| o.discard).collect::<Vec<_>>(),
        vec![None, Some(Discard::Id)]
    );

    // Knob off: the bytes with the known nonce are taken again.
    w.mediator().knobs(|k| k.csp.drop_repeats = false);
    send(&w, &mut d, boxed(&w, PEER, 0xa1, 2));
    assert_eq!(acks(&d).len(), 3);
    assert_eq!(kept_ids(&w), vec![0xa1, 0xa1, 0xa1]);
    assert_eq!(w.mediator().csp_repeats_dropped().len(), 1, "no new drop");
    assert_eq!(
        peer.drain().iter().map(|o| o.discard).collect::<Vec<_>>(),
        vec![Some(Discard::Nonce)]
    );
    w.violations().assert_empty();
}
