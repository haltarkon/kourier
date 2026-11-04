//! The fake mediator, part 1 (`support/mock/d2m.rs`), against the REAL patched `D2mProtocol`:
//! proofs 2, 3, 8, 9 and the D2M half of 14 of lane plan §5.2. The client and the fake run in this
//! thread: the client's datagrams go to `Mediator::receive`, the fake's outbox to
//! `add_datagrams`, until neither side has anything left.

use std::time::Duration;

use kourier_threema::close::{LoggedOut, Stop, Verdict, verdict};
use libthreema::common::config::MediatorServerBaseUrl;
use libthreema::common::keys::DeviceGroupKey;
use libthreema::common::{ChatServerGroup, ClientInfo, D2xDeviceId};
use libthreema::d2m::payload::{IncomingPayload, OutgoingPayload, Reflect, ReflectFlags};
use libthreema::d2m::{D2mContext, D2mProtocol, D2mStateUpdate};
use libthreema::protobuf::{d2d, d2m};
use prost::Message as _;

use crate::support::mock::crypto::{DeviceGroupKeys, open_nonce_ahead};
use crate::support::mock::d2m::{
    ConnId, ConnectRefused, DEFAULT_IDLE_TIMEOUT, HelloSeen, MAX_DATAGRAM, Mediator, Out,
    VOLATILE_KEEP, datagram, kind,
};
use crate::support::mock::{MockWorld, START_MS, world};

pub(crate) const MEDIATOR_TEMPLATE: &str =
    "wss://mediator-{deviceGroupIdPrefix4}.invalid/{deviceGroupIdPrefix8}/";
/// The default server group of the tests: letters and digits only, the mediator's own rule
/// (`TP/md-d2m.proto:37-39`; G6). `a_server_group_with_a_hyphen_is_refused` documents the other.
pub(crate) const SERVER_GROUP: &str = "ds7";
const LABEL: &str = "Kourier (th2-test)";

use d2m::DeviceSlotExpirationPolicy as Expiration;
use d2m::DeviceSlotState as SlotState;
use d2m::client_hello::DeviceSlotsExhaustedPolicy as Exhausted;

/// A device group of the world: its key and what the fake knows of it.
pub(crate) struct Group {
    pub(crate) dgk: [u8; 32],
    pub(crate) keys: DeviceGroupKeys,
}

pub(crate) fn group(w: &MockWorld) -> Group {
    let dgk = w.new_device_group_key();
    let keys = DeviceGroupKeys::derive(&dgk);
    w.mediator().add_group(keys.path_public, SERVER_GROUP);
    Group { dgk, keys }
}

/// How a test device connects.
#[derive(Clone, Copy)]
pub(crate) struct Opts {
    pub(crate) device: u64,
    pub(crate) expected: SlotState,
    pub(crate) exhausted: Exhausted,
    pub(crate) expiration: Expiration,
}

pub(crate) fn new_device(device: u64) -> Opts {
    Opts {
        device,
        expected: SlotState::New,
        exhausted: Exhausted::Reject,
        expiration: Expiration::Persistent,
    }
}

pub(crate) fn existing(device: u64) -> Opts {
    Opts {
        expected: SlotState::Existing,
        ..new_device(device)
    }
}

/// The real client of one connection and what it saw.
pub(crate) struct Client {
    pub(crate) proto: D2mProtocol,
    pub(crate) url: String,
    pub(crate) conn: Option<ConnId>,
    pub(crate) post_handshake: Option<libthreema::d2m::payload::ServerInfo>,
    pub(crate) payloads: Vec<IncomingPayload>,
    pub(crate) closed: Option<u16>,
    /// The client's protocol failed (the fake sent what it must refuse).
    pub(crate) failed: bool,
}

pub(crate) fn client(dgk: [u8; 32], o: Opts) -> Client {
    client_in(dgk, SERVER_GROUP, o)
}

/// A client that names `server_group` (libthreema takes a hyphen the mediator's rule refuses).
pub(crate) fn client_in(dgk: [u8; 32], server_group: &str, o: Opts) -> Client {
    let context = D2mContext {
        mediator_server_url: MediatorServerBaseUrl::try_from(MEDIATOR_TEMPLATE.to_owned())
            .expect("the test's mediator URL"),
        device_group_key: DeviceGroupKey::from(dgk),
        csp_server_group: ChatServerGroup::try_from(server_group).expect("a server group"),
        device_id: D2xDeviceId(o.device),
        device_slots_exhausted_policy: o.exhausted,
        device_slot_expiration_policy: o.expiration,
        expected_device_slot_state: o.expected,
        client_info: ClientInfo::Libthreema,
        device_label: LABEL.to_owned(),
    };
    let (proto, url) = D2mProtocol::new(context);
    Client {
        proto,
        url,
        conn: None,
        post_handshake: None,
        payloads: Vec::new(),
        closed: None,
        failed: false,
    }
}

impl Client {
    pub(crate) fn conn(&self) -> ConnId {
        self.conn.expect("connected")
    }

    pub(crate) fn online(&self) -> bool {
        self.post_handshake.is_some() && self.closed.is_none()
    }
}

/// Moves datagrams both ways until neither side has anything left.
pub(crate) fn pump(m: &Mediator, c: &mut Client) {
    let conn = c.conn();
    loop {
        let outs = m.take(conn);
        let mut sent = false;
        for out in outs {
            match out {
                Out::Datagram(d) => {
                    if c.proto.add_datagrams(vec![d]).is_err() {
                        c.failed = true;
                    }
                }
                Out::Close(code) => c.closed = Some(code),
            }
        }
        while !c.failed && c.closed.is_none() {
            match c.proto.poll() {
                Ok(Some(i)) => {
                    match i.state_update {
                        Some(D2mStateUpdate::PostHandshake(info)) => {
                            c.post_handshake = Some(info);
                        }
                        Some(D2mStateUpdate::AwaitingServerInfo) | None => {}
                    }
                    if let Some(p) = i.incoming_payload {
                        c.payloads.push(p);
                    }
                    if let Some(d) = i.outgoing_datagram {
                        m.receive(conn, d.0);
                        sent = true;
                    }
                }
                Ok(None) => break,
                Err(_) => c.failed = true,
            }
        }
        if !sent {
            return;
        }
    }
}

/// Connects `c` to the world's mediator and runs the handshake as far as it goes.
pub(crate) fn connect(w: &MockWorld, c: &mut Client) {
    c.conn = Some(w.mediator().connect(&c.url).expect("the fake took the URL"));
    pump(w.mediator(), c);
}

pub(crate) fn online(w: &MockWorld, g: &Group, o: Opts) -> Client {
    let mut c = client(g.dgk, o);
    connect(w, &mut c);
    assert!(
        c.online(),
        "the device did not reach the post-handshake phase"
    );
    c
}

/// Sends one payload of the client (encoded by libthreema) to the fake.
pub(crate) fn send(w: &MockWorld, c: &mut Client, payload: OutgoingPayload) {
    let d = c
        .proto
        .create_payload(payload)
        .expect("the client encodes the payload")
        .outgoing_datagram
        .expect("a datagram");
    w.mediator().receive(c.conn(), d.0);
    pump(w.mediator(), c);
}

// --- Proof 2: the handshake ------------------------------------------------------------------

/// The URL's `ClientUrlInfo` is the group's; the challenge response opens; `ServerInfo` brings the
/// real client to the post-handshake phase with what the fake said; the fake keeps the device info
/// it cannot read, and the test opens it with DGDIK.
#[test]
fn the_real_client_completes_the_handshake_and_registers_its_slot() {
    let w = world(21);
    let g = group(&w);
    // An id above i64::MAX (the u64 device ids of KT1, S-mock §13 risk 5).
    let id = 0xF000_0000_0000_0021;
    let c = online(&w, &g, new_device(id));
    let info = c.post_handshake.as_ref().expect("ServerInfo");
    assert_eq!(info.max_device_slots, 4, "max_device_slots");
    assert!(
        info.device_slot_state == Some(SlotState::New),
        "a first connect is told its slot is NEW"
    );
    assert_eq!(info.reflection_queue_length, 0, "an empty queue");
    assert!(
        info.encrypted_shared_device_data.is_empty(),
        "no shared data yet"
    );
    let m = w.mediator();
    assert!(m.is_online(c.conn()), "the fake does not count it online");
    assert_eq!(m.device_of(c.conn()), Some(id), "the device id");
    assert_eq!(m.slots(&g.keys.path_public), vec![id], "the slot");
    let sealed = m
        .device_info(&g.keys.path_public, id)
        .expect("the device info is kept");
    let plain =
        open_nonce_ahead(&g.keys.device_info, &sealed).expect("the device info opens with DGDIK");
    let device_info = d2d::DeviceInfo::decode(plain.as_slice()).expect("a DeviceInfo");
    assert!(device_info.label == LABEL, "the label differs");
    assert!(!c.failed, "the client refused something");
    w.violations().assert_empty();
}

/// `ServerInfo.current_time` is the virtual clock plus the skew knob (the client's 20-minute check
/// of TH3 is tested against it); the real client still reaches the post-handshake phase.
#[test]
fn server_info_carries_the_virtual_clock_with_the_skew_knob() {
    for skew in [0_i64, 21 * 60_000, -21 * 60_000] {
        let w = world(22);
        let g = group(&w);
        w.mediator().knobs(|k| k.clock_skew_ms = skew);
        let mut c = client(g.dgk, new_device(22));
        let m = w.mediator();
        let conn = m.connect(&c.url).expect("connect");
        c.conn = Some(conn);
        let hello = m.take(conn);
        let [Out::Datagram(hello)] = hello.as_slice() else {
            panic!("the fake's first message is not one ServerHello");
        };
        assert_eq!(hello[0], kind::SERVER_HELLO, "the first message's type");
        c.proto.add_datagrams(vec![hello.clone()]).expect("add");
        let i = c.proto.poll().expect("poll").expect("a ClientHello");
        m.receive(conn, i.outgoing_datagram.expect("a ClientHello").0);
        let outs = m.take(conn);
        // ServerInfo, then Dry and the promotion (M3: the only device of the group leads).
        let [
            Out::Datagram(info),
            Out::Datagram(dry),
            Out::Datagram(promoted),
        ] = outs.as_slice()
        else {
            panic!("the fake did not answer with ServerInfo, Dry and the promotion");
        };
        assert_eq!(
            (dry[0], promoted[0]),
            (kind::REFLECTION_QUEUE_DRY, kind::ROLE_PROMOTED_TO_LEADER),
            "what follows the ServerInfo"
        );
        assert_eq!(info[0], kind::SERVER_INFO, "the answer's type");
        let decoded = d2m::ServerInfo::decode(&info[4..]).expect("a ServerInfo");
        assert_eq!(
            i128::from(decoded.current_time),
            i128::from(START_MS) + i128::from(skew),
            "current_time with skew {skew}"
        );
        c.proto.add_datagrams(vec![info.clone()]).expect("add");
        let i = c.proto.poll().expect("poll").expect("PostHandshake");
        assert!(
            matches!(i.state_update, Some(D2mStateUpdate::PostHandshake(_))),
            "the client did not take the ServerInfo"
        );
        w.violations().assert_empty();
    }
}

/// A client with another device group key names a group the fake does not know: refused at the
/// connect, and counted.
#[test]
fn a_wrong_device_group_key_is_refused_at_the_connect() {
    let w = world(23);
    let _g = group(&w);
    let stranger = client(w.new_device_group_key(), new_device(23));
    assert_eq!(
        w.mediator().connect(&stranger.url),
        Err(ConnectRefused::UnknownDeviceGroup),
        "an unknown group was taken"
    );
    for bad in [
        "wss://mediator.example/ab/00",
        "wss://mediator-a.invalid/ab/zz",
        "wss://mediator-a.invalid/ab/",
    ] {
        assert!(w.mediator().connect(bad).is_err(), "a bad URL was taken");
    }
    // The stranger and the three bad URLs: four refusals, all counted (the check is the last
    // statement, G12).
    assert_eq!(w.violations().rules(), vec!["mediator-connect-refused"; 4]);
}

/// The URL of one group with the key of another: the challenge response does not open under the
/// URL's DGPK and the fake closes with 4010 (D4; the code is [U: real server]).
#[test]
fn a_challenge_response_under_another_key_is_closed_with_4010() {
    let w = world(24);
    let (g, other) = (group(&w), group(&w));
    let mut c = client(g.dgk, new_device(24));
    c.url = client(other.dgk, new_device(24)).url;
    connect(&w, &mut c);
    assert_eq!(c.closed, Some(4010), "the close code");
    assert!(c.post_handshake.is_none(), "the client got a ServerInfo");
    assert!(
        w.mediator().slots(&other.keys.path_public).is_empty(),
        "a slot was registered"
    );
    assert_eq!(
        w.violations().rules(),
        vec!["challenge-response-does-not-open"]
    );
}

/// A `ClientHello` with a version above the server's: 4110 (the spec's "Version Negotiation").
#[test]
fn a_client_version_above_the_servers_is_closed_with_4110() {
    let w = world(25);
    let g = group(&w);
    let m = w.mediator();
    let conn = m
        .connect(&client(g.dgk, new_device(25)).url)
        .expect("connect");
    let _ = m.take(conn);
    let hello = d2m::ClientHello {
        version: 1,
        ..Default::default()
    };
    m.receive(conn, datagram(kind::CLIENT_HELLO, &hello.encode_to_vec()));
    assert_eq!(m.take(conn), vec![Out::Close(4110)], "the answer");
    assert_eq!(
        w.violations().rules(),
        vec!["client-version-above-the-servers"]
    );
}

// --- Proof 3: slots ----------------------------------------------------------------------------

/// REJECT with every slot used: 4111 for a new device; a known device still gets in.
#[test]
fn a_new_device_with_every_slot_used_is_rejected_with_4111() {
    let w = world(31);
    let g = group(&w);
    w.mediator().knobs(|k| k.max_device_slots = 2);
    for id in [1, 2] {
        let c = online(&w, &g, new_device(id));
        w.mediator().client_close(c.conn(), 1000);
    }
    let mut third = client(g.dgk, new_device(3));
    connect(&w, &mut third);
    assert_eq!(third.closed, Some(4111), "the third device's close code");
    assert_eq!(
        w.mediator().slots(&g.keys.path_public),
        vec![1, 2],
        "the slots"
    );
    let back = online(&w, &g, existing(2));
    assert!(
        back.post_handshake
            .as_ref()
            .expect("info")
            .device_slot_state
            == Some(SlotState::Existing),
        "a known device is told EXISTING"
    );
    w.violations().assert_empty();
}

/// The expected slot state must be the real one, both ways: 4115 before any registration.
#[test]
fn an_expected_slot_state_that_is_not_the_real_one_is_closed_with_4115() {
    let w = world(32);
    let g = group(&w);
    let mut fresh = client(g.dgk, existing(5));
    connect(&w, &mut fresh);
    assert_eq!(fresh.closed, Some(4115), "EXISTING expected for a new slot");
    assert!(
        w.mediator().slots(&g.keys.path_public).is_empty(),
        "the mismatch registered a slot"
    );
    let first = online(&w, &g, new_device(5));
    w.mediator().client_close(first.conn(), 1000);
    let mut again = client(g.dgk, new_device(5));
    connect(&w, &mut again);
    assert_eq!(again.closed, Some(4115), "NEW expected for a known slot");
    w.violations().assert_empty();
}

/// The same device connects again: the OLDER connection gets 4112, the newer one goes on.
#[test]
fn a_second_connection_of_one_device_closes_the_older_with_4112() {
    let w = world(33);
    let g = group(&w);
    let mut older = online(&w, &g, new_device(7));
    let newer = online(&w, &g, existing(7));
    pump(w.mediator(), &mut older);
    assert_eq!(
        older.closed,
        Some(4112),
        "the older connection's close code"
    );
    assert!(newer.online(), "the newer connection was closed");
    assert!(
        w.mediator().is_online(newer.conn()),
        "the fake dropped the newer one"
    );
    w.violations().assert_empty();
}

/// DROP_LEAST_RECENT is a violation (M6: Kourier never sends it); the fake drops nobody.
#[test]
fn drop_least_recent_is_a_violation_and_drops_nobody() {
    let w = world(34);
    let g = group(&w);
    w.mediator().knobs(|k| k.max_device_slots = 1);
    let one = online(&w, &g, new_device(1));
    let mut two = client(
        g.dgk,
        Opts {
            exhausted: Exhausted::DropLeastRecent,
            ..new_device(2)
        },
    );
    connect(&w, &mut two);
    assert_eq!(two.closed, Some(4111), "the fake did not refuse as REJECT");
    assert!(
        w.mediator().is_online(one.conn()),
        "the fake dropped a device"
    );
    assert_eq!(w.violations().rules(), vec!["drop-least-recent-policy"]);
}

/// A VOLATILE slot goes some time after its last connection; a PERSISTENT one stays.
#[test]
fn a_volatile_slot_goes_after_its_keep_time_and_a_persistent_one_stays() {
    let w = world(35);
    let g = group(&w);
    let v = online(
        &w,
        &g,
        Opts {
            expiration: Expiration::Volatile,
            ..new_device(8)
        },
    );
    let p = online(&w, &g, new_device(9));
    w.mediator().client_close(v.conn(), 1000);
    w.mediator().client_close(p.conn(), 1000);
    w.mediator()
        .advance(VOLATILE_KEEP - Duration::from_millis(1));
    assert_eq!(
        w.mediator().slots(&g.keys.path_public),
        vec![8, 9],
        "too early"
    );
    w.mediator().advance(Duration::from_millis(1));
    assert_eq!(
        w.mediator().slots(&g.keys.path_public),
        vec![9],
        "after the keep time"
    );
    w.violations().assert_empty();
}

// --- Proof 8: close codes ----------------------------------------------------------------------

/// Every close code of D8 that the fake can send reaches the real client's connection, and
/// `src/close.rs` maps it (the expectations written out here, not taken from the table).
#[test]
fn every_close_code_reaches_the_client_and_close_rs_maps_it() {
    let reconnect = Verdict::Reconnect { warn: false };
    let codes: [(u16, Verdict); 16] = [
        (1000, reconnect),
        (1001, reconnect),
        (1011, reconnect),
        (4000, reconnect),
        (4001, reconnect),
        (4009, reconnect),
        (4010, reconnect),
        (4011, reconnect),
        (4012, reconnect),
        (4013, reconnect),
        (4110, Verdict::Stop(Stop::UpdateApp)),
        (4111, Verdict::Stop(Stop::TooManyDevices)),
        (4112, Verdict::Stop(Stop::Reconnect)),
        (4113, Verdict::LoggedOut(LoggedOut::DroppedByOtherDevice)),
        (4114, Verdict::LoggedOut(LoggedOut::QueueOverflow)),
        (4115, Verdict::LoggedOut(LoggedOut::SlotStateMismatch)),
    ];
    let w = world(81);
    let g = group(&w);
    // One slot per code (the fake's default is 4).
    w.mediator().knobs(|k| k.max_device_slots = 32);
    for (n, (code, want)) in codes.into_iter().enumerate() {
        let id = 100 + u64::try_from(n).expect("small");
        let mut c = online(&w, &g, new_device(id));
        w.mediator().close(c.conn(), code);
        pump(w.mediator(), &mut c);
        assert_eq!(c.closed, Some(code), "the client did not see {code}");
        assert_eq!(verdict(code, None), want, "close.rs maps {code}");
    }
    // A close during the handshake (C14: the chat server could not be reached).
    let mut early = client(g.dgk, new_device(1));
    early.conn = Some(w.mediator().connect(&early.url).expect("connect"));
    w.mediator().close(early.conn(), 4001);
    pump(w.mediator(), &mut early);
    assert_eq!(early.closed, Some(4001), "a close before the ClientHello");
    w.violations().assert_empty();
}

/// The client's own close codes the spec and Desktop use are fine (1000; 4010 and 4013; 4110 for a
/// server version it cannot speak, `md-d2m.struct.yml:71-73`); another is a violation (G4).
#[test]
fn the_client_may_close_with_1000_4010_4013_or_4110_only() {
    let w = world(82);
    let g = group(&w);
    // Five devices in all: more slots than the fake's default four.
    w.mediator().knobs(|k| k.max_device_slots = 8);
    for (n, code) in [1000_u16, 4010, 4013, 4110].into_iter().enumerate() {
        let c = online(&w, &g, new_device(10 + u64::try_from(n).expect("small")));
        w.mediator().client_close(c.conn(), code);
        assert_eq!(w.mediator().closed(c.conn()), Some(code), "{code}");
    }
    w.violations().assert_empty();
    let c = online(&w, &g, new_device(20));
    w.mediator().client_close(c.conn(), 4115);
    assert_eq!(w.violations().rules(), vec!["client-close-code"]);
}

// --- Proof 9: idle -----------------------------------------------------------------------------

/// No frame for the idle timeout on the virtual clock: 4013; every frame starts the time again; a
/// connection's own mediator timeout (the script call `set_idle_timeout`) counts instead. The CHAT
/// SERVER's timer, set by CSP's `set-connection-idle-timeout`, is another one (`mock::csp`).
#[test]
fn an_idle_connection_is_closed_with_4013_on_the_virtual_clock() {
    let w = world(91);
    let g = group(&w);
    let mut c = online(&w, &g, new_device(1));
    let ms = Duration::from_millis(1);
    w.mediator().advance(DEFAULT_IDLE_TIMEOUT - ms);
    pump(w.mediator(), &mut c);
    assert!(c.online(), "closed before the timeout");
    send(&w, &mut c, OutgoingPayload::Proxy(vec![0]));
    w.mediator().advance(DEFAULT_IDLE_TIMEOUT - ms);
    pump(w.mediator(), &mut c);
    assert!(c.online(), "a frame did not start the time again");
    // The mediator's timer counts ANY frame: a D2M reflect starts it again too (the chat server's
    // timer does not, `mock::csp::d2m_frames_alone_do_not_keep_the_chat_servers_idle_timer_alive`).
    send(
        &w,
        &mut c,
        OutgoingPayload::Reflect(Reflect {
            flags: ReflectFlags(0),
            reflect_id: 1,
            envelope: vec![1; 40],
        }),
    );
    w.mediator().advance(DEFAULT_IDLE_TIMEOUT - ms);
    pump(w.mediator(), &mut c);
    assert!(c.online(), "a reflect did not start the time again");
    w.mediator().advance(ms);
    pump(w.mediator(), &mut c);
    assert_eq!(c.closed, Some(4013), "the idle close");

    let mut short = online(&w, &g, new_device(2));
    w.mediator()
        .set_idle_timeout(short.conn(), Duration::from_secs(30));
    w.mediator().advance(Duration::from_secs(30));
    pump(w.mediator(), &mut short);
    assert_eq!(short.closed, Some(4013), "the connection's own timeout");
    w.violations().assert_empty();
}

// --- Proof 14 (D2M half): the size limit, both sides -----------------------------------------

/// 65,536 bytes per D2M message (the container: type, reserved and payload) are taken; 65,537 are
/// refused with 4010 and recorded. The client's own encoder makes the frames.
#[test]
fn a_d2m_message_of_65536_bytes_is_taken_and_one_of_65537_refused() {
    let w = world(141);
    // The payloads here are no CSP: the mediator alone is under test (the CSP limit is
    // `mock::csp::a_csp_frame_of_8192_bytes_is_taken_and_one_of_8193_refused`).
    w.mediator().knobs(|k| k.chat_server = false);
    let g = group(&w);
    let mut c = online(&w, &g, new_device(1));
    let biggest = MAX_DATAGRAM - 4;
    let d = c
        .proto
        .create_payload(OutgoingPayload::Proxy(vec![7; biggest]))
        .expect("encode")
        .outgoing_datagram
        .expect("a datagram")
        .0;
    assert_eq!(d.len(), 65_536, "the client's frame");
    w.mediator().receive(c.conn(), d);
    pump(w.mediator(), &mut c);
    assert!(c.online(), "a frame at the limit was refused");
    assert_eq!(
        w.mediator().proxied(c.conn()).last().map(Vec::len),
        Some(biggest),
        "the payload at the limit did not arrive whole"
    );
    w.violations().assert_empty();

    let d = c
        .proto
        .create_payload(OutgoingPayload::Proxy(vec![7; biggest + 1]))
        .expect("encode")
        .outgoing_datagram
        .expect("a datagram")
        .0;
    assert_eq!(d.len(), 65_537, "the client's frame");
    w.mediator().receive(c.conn(), d);
    pump(w.mediator(), &mut c);
    assert_eq!(c.closed, Some(4010), "a frame over the limit was taken");
    assert_eq!(w.violations().rules(), vec!["d2m-message-over-65536-bytes"]);
}

/// Under the container's header, or a type the client must not send: 4010 and recorded. The
/// reserved bytes are ignored.
#[test]
fn a_short_message_or_a_servers_type_from_the_client_is_a_protocol_error() {
    let w = world(142);
    let g = group(&w);
    let c = online(&w, &g, new_device(1));
    w.mediator().receive(c.conn(), vec![0, 1, 2, 3, 9]);
    assert_eq!(
        w.mediator().proxied(c.conn()).last().map(Vec::as_slice),
        Some(&[9u8][..]),
        "nonzero reserved bytes were not ignored"
    );
    w.mediator().receive(c.conn(), vec![0, 0, 0]);
    assert_eq!(w.mediator().closed(c.conn()), Some(4010), "a short message");
    let other = online(&w, &g, new_device(2));
    w.mediator()
        .receive(other.conn(), datagram(kind::SERVER_INFO, &[]));
    assert_eq!(
        w.mediator().closed(other.conn()),
        Some(4010),
        "a server's type"
    );
    assert_eq!(
        w.violations().rules(),
        vec![
            "d2m-message-under-the-header",
            "d2m-unexpected-payload-type"
        ]
    );
}

// --- Proof 12 (first half): a Proxy before the ClientHello ------------------------------------

/// A Proxy the client sends before its ClientHello (Desktop sends its CSP client-hello so early)
/// is held and handed on, in order, when the handshake ends.
#[test]
fn a_proxy_before_the_client_hello_is_held_not_lost() {
    let w = world(121);
    let g = group(&w);
    let mut c = client(g.dgk, new_device(1));
    let m = w.mediator();
    c.conn = Some(m.connect(&c.url).expect("connect"));
    for byte in [1u8, 2] {
        let d = c
            .proto
            .create_payload(OutgoingPayload::Proxy(vec![byte; 48]))
            .expect("the client may proxy before the handshake")
            .outgoing_datagram
            .expect("a datagram");
        m.receive(c.conn(), d.0);
    }
    assert_eq!(m.held_proxy_count(c.conn()), 2, "held");
    assert!(
        m.proxied(c.conn()).is_empty(),
        "handed on before the handshake"
    );
    pump(m, &mut c);
    assert!(c.online(), "the handshake");
    assert_eq!(
        m.proxied(c.conn()),
        vec![vec![1u8; 48], vec![2u8; 48]],
        "the held payloads"
    );
    assert_eq!(m.held_proxy_count(c.conn()), 0, "still held");
    assert!(
        !c.payloads.iter().any(|p| !matches!(
            p,
            IncomingPayload::Proxy(_)
                | IncomingPayload::ReflectionQueueDry
                | IncomingPayload::RolePromotedToLeader
        )),
        "the fake sent the client something unexpected"
    );
    w.violations().assert_empty();
}

// --- The URL's 8-bit prefix (G15 patch P9, G5) --------------------------------------------------

/// A device group key whose DGPK.public starts below 0x10 (one group in 16).
fn group_below_0x10(w: &MockWorld) -> Group {
    loop {
        let dgk = w.new_device_group_key();
        let keys = DeviceGroupKeys::derive(&dgk);
        if keys.path_public[0] < 0x10 {
            w.mediator().add_group(keys.path_public, SERVER_GROUP);
            return Group { dgk, keys };
        }
    }
}

/// `{deviceGroupIdPrefix8}` is the first byte as TWO hex digits (`oppf.schema.yml`: "the first two
/// hex nibbles"; Desktop's `slice(0, 2)`). libthreema wrote ONE digit below 0x10
/// (`format!("{prefix:x}")`) until the vendored patch P9: this pins the patched form, so a copy
/// that loses the patch fails here and in libthreema's own suite. The fake takes it and the
/// handshake completes.
#[test]
fn libthreemas_url_keeps_the_leading_zero_of_prefix8() {
    let w = world(151);
    let g = group_below_0x10(&w);
    let c = client(g.dgk, new_device(1));
    let prefix8 = c.url.split('/').nth(3).expect("a path");
    assert_eq!(prefix8.len(), 2, "libthreema wrote a one-digit prefix");
    assert!(prefix8.starts_with('0'), "the first nibble");
    let c = online(&w, &g, new_device(1));
    assert!(c.online(), "the handshake");
    w.violations().assert_empty();
}

/// G5: a one-digit prefix8 for a first byte below 0x10 (the form of an unpatched libthreema, built
/// by hand here) is refused with its OWN rule, so a failure says why.
/// [U: real server] which form the real mediator routes: the two-digit one is assumed.
#[test]
fn a_one_digit_prefix8_is_refused_with_its_own_rule() {
    let w = world(153);
    let g = group_below_0x10(&w);
    let url = client(g.dgk, new_device(1)).url;
    let parts: Vec<&str> = url.split('/').collect();
    let one_digit = [parts[0], parts[1], parts[2], &parts[3][1..], parts[4]].join("/");
    assert_ne!(one_digit, url, "the hand-made URL");
    assert_eq!(
        w.mediator().connect(&one_digit),
        Err(ConnectRefused::PrefixEightHasOneDigit),
        "the fake took a one-digit prefix"
    );
    assert_eq!(
        w.violations().rules(),
        vec!["mediator-url-prefix8-one-digit"]
    );
}

/// G7: the `ClientUrlInfo` hex is lowercase (`md-d2m.proto:26`); an upper-case URL is refused with
/// its own rule. [I]
#[test]
fn an_upper_case_client_url_info_is_refused() {
    let w = world(154);
    let g = group(&w);
    let url = client(g.dgk, new_device(1)).url;
    let (base, hex) = url.rsplit_once('/').expect("a path");
    let upper = format!("{base}/{}", hex.to_uppercase());
    assert_ne!(upper, url, "the URL has no letter to upper-case");
    assert_eq!(
        w.mediator().connect(&upper),
        Err(ConnectRefused::UpperCaseClientUrlInfo),
        "the fake took an upper-case URL"
    );
    assert_eq!(
        w.violations().rules(),
        vec!["mediator-url-client-url-info-not-lowercase"]
    );
}

/// G6: the mediator's own rule for the server group is `^[0-9a-zA-Z]+$` (`md-d2m.proto:37-39`);
/// the join protocol and libthreema also take a hyphen. The fake follows the mediator's rule and
/// refuses a hyphenated group even when it is the group's own. [U: real server] RT1 shows which
/// the real mediator applies.
#[test]
fn a_server_group_with_a_hyphen_is_refused() {
    let w = world(155);
    let dgk = w.new_device_group_key();
    let keys = DeviceGroupKeys::derive(&dgk);
    w.mediator().add_group(keys.path_public, "ds-7");
    let c = client_in(dgk, "ds-7", new_device(1));
    assert_eq!(
        w.mediator().connect(&c.url),
        Err(ConnectRefused::BadServerGroup),
        "the fake took a hyphenated server group"
    );
    assert_eq!(w.violations().rules(), vec!["mediator-connect-refused"]);
}

// --- G1: a slot with an open connection stays ---------------------------------------------------

/// G1 (the spec review's MEDIUM-1): a device that reconnects (4112 for its older connection) is
/// not "disconnected": its VOLATILE slot must not be deleted while the newer connection is open,
/// not even after the keep time that the older connection's close started.
#[test]
fn a_volatile_slot_stays_while_its_device_is_online_after_a_4112_takeover() {
    let w = world(36);
    let g = group(&w);
    // Longer than the keep time, so that the newer connection is not idle-closed first.
    w.mediator()
        .knobs(|k| k.idle_timeout = Duration::from_secs(3600));
    let volatile = Opts {
        expiration: Expiration::Volatile,
        ..new_device(7)
    };
    let mut older = online(&w, &g, volatile);
    let newer = online(
        &w,
        &g,
        Opts {
            expected: SlotState::Existing,
            ..volatile
        },
    );
    pump(w.mediator(), &mut older);
    assert_eq!(older.closed, Some(4112), "the older connection");
    w.mediator()
        .advance(VOLATILE_KEEP + Duration::from_millis(1));
    assert!(
        w.mediator().is_online(newer.conn()),
        "the newer connection is gone"
    );
    assert_eq!(
        w.mediator().slots(&g.keys.path_public),
        vec![7],
        "the VOLATILE slot of a connected device was deleted"
    );
    // Once the device really goes, the keep time starts.
    w.mediator().client_close(newer.conn(), 1000);
    w.mediator()
        .advance(VOLATILE_KEEP - Duration::from_millis(1));
    assert_eq!(
        w.mediator().slots(&g.keys.path_public),
        vec![7],
        "too early"
    );
    w.mediator().advance(Duration::from_millis(1));
    assert!(
        w.mediator().slots(&g.keys.path_public).is_empty(),
        "the slot after the keep time"
    );
    w.violations().assert_empty();
}

// --- G11: a scripted 4113 / 4114 frees the slot -------------------------------------------------

/// G11: the close codes 4113 (dropped by another device) and 4114 (dropped by the server) free the
/// device's slot, as `DropDevice` and the queue limit do: a reconnect with EXISTING is told 4115,
/// a new registration works; any other scripted code keeps the slot.
#[test]
fn a_scripted_4113_or_4114_frees_the_slot_so_a_reconnect_is_told_4115() {
    for (seed, code) in [(37_u64, 4113_u16), (38, 4114)] {
        let w = world(seed);
        let g = group(&w);
        let mut dropped = online(&w, &g, new_device(5));
        let kept = online(&w, &g, new_device(6));
        w.mediator().close(dropped.conn(), code);
        w.mediator().close(kept.conn(), 4000);
        pump(w.mediator(), &mut dropped);
        assert_eq!(dropped.closed, Some(code), "the client's close code");
        assert_eq!(
            w.mediator().slots(&g.keys.path_public),
            vec![6],
            "only the dropped device lost its slot ({code})"
        );
        let mut again = client(g.dgk, existing(5));
        connect(&w, &mut again);
        assert_eq!(
            again.closed,
            Some(4115),
            "a reconnect with EXISTING after {code}"
        );
        let back = online(&w, &g, new_device(5));
        assert!(back.online(), "a fresh registration after {code}");
        w.violations().assert_empty();
    }
}

/// TH3 (critic H2): with `hold_dry` the login's Dry and the promotion after it wait for
/// `release_dry`, which sends both in that order once; a connection that got its Dry has nothing
/// held. Without the knob nothing changes (Dry at once).
#[test]
fn a_held_dry_and_its_promotion_come_only_at_the_release() {
    let w = world(39);
    let g = group(&w);
    w.mediator().knobs(|k| k.hold_dry = true);
    let mut c = online(&w, &g, new_device(5));
    let dry_or_role = |c: &Client| {
        c.payloads
            .iter()
            .filter(|p| {
                matches!(
                    p,
                    IncomingPayload::ReflectionQueueDry | IncomingPayload::RolePromotedToLeader
                )
            })
            .count()
    };
    assert_eq!(dry_or_role(&c), 0, "Dry or the promotion came while held");
    assert!(
        !w.mediator().is_leader(c.conn()),
        "promoted while Dry is held"
    );
    assert!(w.mediator().release_dry(c.conn()), "nothing was held");
    pump(w.mediator(), &mut c);
    assert!(
        matches!(
            c.payloads.as_slice(),
            [
                IncomingPayload::ReflectionQueueDry,
                IncomingPayload::RolePromotedToLeader
            ]
        ),
        "the release did not send Dry, then the promotion"
    );
    assert!(
        !w.mediator().release_dry(c.conn()),
        "a second release sent more"
    );
    w.mediator().knobs(|k| k.hold_dry = false);
    let mut other = online(&w, &g, new_device(6));
    pump(w.mediator(), &mut other);
    assert_eq!(
        dry_or_role(&other),
        1,
        "Dry without the knob (the leader stays)"
    );
    assert!(
        !w.mediator().release_dry(other.conn()),
        "held without the knob"
    );
    w.violations().assert_empty();
}

/// TH3: the fake keeps what each `ClientHello` said (the device, the expected slot state and both
/// policies) for the actor's tests.
#[test]
fn the_fake_keeps_what_the_client_hello_said() {
    let w = world(40);
    let g = group(&w);
    let first = online(&w, &g, new_device(7));
    assert_eq!(
        w.mediator().hello(first.conn()),
        Some(HelloSeen {
            device: 7,
            expected: SlotState::New,
            exhausted: Exhausted::Reject,
            expiration: Expiration::Persistent,
        })
    );
    w.mediator().close(first.conn(), 4000);
    let again = online(&w, &g, existing(7));
    assert!(
        w.mediator().hello(again.conn()).map(|h| h.expected) == Some(SlotState::Existing),
        "the second hello's expected state"
    );
    w.violations().assert_empty();
}

/// TH3 (critic M4): `drop_slot_at_next_4115` makes the NEXT slot-state mismatch also delete the
/// slot, once: a retry with EXISTING is then told 4115 too; the knob is off afterwards, so a later
/// mismatch keeps the slot.
#[test]
fn the_next_4115_can_drop_the_slot_once() {
    let w = world(41);
    let g = group(&w);
    let first = online(&w, &g, new_device(5));
    w.mediator().client_close(first.conn(), 1000);
    assert_eq!(w.mediator().slots(&g.keys.path_public), vec![5]);
    w.mediator().knobs(|k| k.drop_slot_at_next_4115 = true);
    let mut stale = client(g.dgk, new_device(5));
    connect(&w, &mut stale);
    assert_eq!(stale.closed, Some(4115), "NEW expected for a known slot");
    assert!(
        w.mediator().slots(&g.keys.path_public).is_empty(),
        "the knob did not drop the slot"
    );
    let mut retry = client(g.dgk, existing(5));
    connect(&w, &mut retry);
    assert_eq!(retry.closed, Some(4115), "EXISTING expected after the drop");
    let again = online(&w, &g, new_device(5));
    w.mediator().client_close(again.conn(), 1000);
    let mut later = client(g.dgk, new_device(5));
    connect(&w, &mut later);
    assert_eq!(later.closed, Some(4115), "NEW expected for a known slot");
    assert_eq!(
        w.mediator().slots(&g.keys.path_public),
        vec![5],
        "the knob dropped a second slot"
    );
    w.violations().assert_empty();
}

// --- RP-b: DropDevice -------------------------------------------------------------------------

/// The datagrams and the close the fake sent on `conn`, the client's protocol fed with the datagrams; the payloads
/// libthreema read from them, in order, and the close code.
fn read_all(w: &MockWorld, c: &mut Client) -> (Vec<IncomingPayload>, Option<u16>) {
    let mut closed = None;
    for out in w.mediator().take(c.conn()) {
        match out {
            Out::Datagram(d) => c
                .proto
                .add_datagrams(vec![d])
                .expect("the client takes the datagram"),
            Out::Close(code) => closed = Some(code),
        }
    }
    let mut payloads = Vec::new();
    while let Some(i) = c.proto.poll().expect("the client reads the datagrams") {
        payloads.extend(i.incoming_payload);
    }
    (payloads, closed)
}

/// RP-b (D1): a device that drops ITSELF gets `DropDeviceAck` with its id and then 4113; its slot is freed (a
/// reconnect with EXISTING is told 4115) and the other device keeps its own and stays online. A device that drops
/// ANOTHER gets the ack and stays online, the other one is closed 4113. Every drop is recorded with its connection
/// and the id it named.
#[test]
fn a_drop_device_is_acked_frees_the_slot_and_closes_the_dropped_device_with_4113() {
    let w = world(43);
    let g = group(&w);
    let mut me = online(&w, &g, new_device(5));
    let other = online(&w, &g, new_device(6));
    let d = me
        .proto
        .create_payload(OutgoingPayload::DropDevice(D2xDeviceId(5)))
        .expect("the client encodes the drop")
        .outgoing_datagram
        .expect("a datagram");
    w.mediator().receive(me.conn(), d.0);
    let (payloads, closed) = read_all(&w, &mut me);
    assert!(
        matches!(
            payloads.as_slice(),
            [IncomingPayload::DropDeviceAck(D2xDeviceId(5))]
        ),
        "not exactly the ack of the own id"
    );
    assert_eq!(closed, Some(4113), "the dropped device was not closed 4113");
    assert_eq!(
        w.mediator().slots(&g.keys.path_public),
        vec![6],
        "the slot was not freed, or another went"
    );
    assert!(
        w.mediator().is_online(other.conn()),
        "the other device went"
    );
    let mut again = client(g.dgk, existing(5));
    connect(&w, &mut again);
    assert_eq!(
        again.closed,
        Some(4115),
        "a reconnect with EXISTING after the drop"
    );
    let mut dropper = online(&w, &g, new_device(7));
    send(
        &w,
        &mut dropper,
        OutgoingPayload::DropDevice(D2xDeviceId(6)),
    );
    // (The dropper is promoted right after the ack: the dropped device led.)
    assert!(
        dropper
            .payloads
            .iter()
            .any(|p| matches!(p, IncomingPayload::DropDeviceAck(D2xDeviceId(6)))),
        "the dropper did not get the ack of the other id"
    );
    assert!(dropper.online(), "the dropper was closed");
    assert_eq!(
        w.mediator().closed(other.conn()),
        Some(4113),
        "the dropped device was not closed 4113"
    );
    assert_eq!(w.mediator().slots(&g.keys.path_public), vec![7]);
    let seen: Vec<(ConnId, u64)> = w
        .mediator()
        .drops()
        .iter()
        .map(|d| (d.conn, d.device))
        .collect();
    assert_eq!(
        seen,
        vec![(me.conn(), 5), (dropper.conn(), 6)],
        "the recorded drops"
    );
    w.violations().assert_empty();
}

/// RP-b: under `withhold_drop_ack` the drop is recorded and nothing follows (no ack, the slot kept, the connection
/// open); a malformed `DropDevice` is a protocol error (4010, recorded).
#[test]
fn a_withheld_drop_answers_nothing_and_a_malformed_one_is_a_protocol_error() {
    let w = world(44);
    let g = group(&w);
    w.mediator().knobs(|k| k.withhold_drop_ack = true);
    let mut me = online(&w, &g, new_device(5));
    send(&w, &mut me, OutgoingPayload::DropDevice(D2xDeviceId(5)));
    assert!(
        !me.payloads
            .iter()
            .any(|p| matches!(p, IncomingPayload::DropDeviceAck(_))),
        "a withheld drop was acked"
    );
    assert!(me.online(), "a withheld drop closed the connection");
    assert_eq!(
        w.mediator().slots(&g.keys.path_public),
        vec![5],
        "the slot went"
    );
    assert_eq!(w.mediator().drops().len(), 1, "the drop was not recorded");
    w.mediator()
        .receive(me.conn(), datagram(kind::DROP_DEVICE, &[0xff]));
    assert_eq!(
        w.mediator().closed(me.conn()),
        Some(4010),
        "a malformed drop"
    );
    assert_eq!(w.violations().rules(), vec!["drop-device-malformed"]);
}

/// RP-b (P-M1): `close_at_next_drop` ends the dropping connection with its code INSTEAD of an answer (recorded, no ack,
/// the slot kept), once: the next drop is acked and frees the slot as always.
/// RP-b (P-M2): `register_slot_after_next_4115` makes the NEXT 4115 for a device without a slot also register it, once
/// (a late NEW hello): a second EXISTING hello finds the slot; a mismatch after that makes none.
#[test]
fn the_fake_can_close_a_drop_and_register_a_slot_after_a_4115_once_each() {
    let w = world(45);
    let g = group(&w);
    w.mediator().knobs(|k| k.close_at_next_drop = Some(4000));
    let mut me = online(&w, &g, new_device(5));
    send(&w, &mut me, OutgoingPayload::DropDevice(D2xDeviceId(5)));
    assert!(
        !me.payloads
            .iter()
            .any(|p| matches!(p, IncomingPayload::DropDeviceAck(_))),
        "a closed drop was acked"
    );
    assert_eq!(me.closed, Some(4000), "not closed with the knob's code");
    assert_eq!(
        w.mediator().slots(&g.keys.path_public),
        vec![5],
        "a closed drop freed the slot"
    );
    assert_eq!(w.mediator().drops().len(), 1, "the drop was not recorded");
    let mut again = online(&w, &g, existing(5));
    send(&w, &mut again, OutgoingPayload::DropDevice(D2xDeviceId(5)));
    // (The ack and the close come in one pump, which stops reading at the close: the freed slot and the 4113 say it.)
    assert_eq!(
        again.closed,
        Some(4113),
        "the second drop was not answered (the knob is once)"
    );
    assert!(
        w.mediator().slots(&g.keys.path_public).is_empty(),
        "the second drop did not free the slot (the knob is once)"
    );
    assert_eq!(
        w.mediator().drops().len(),
        2,
        "the second drop was not recorded"
    );

    w.mediator()
        .knobs(|k| k.register_slot_after_next_4115 = true);
    let mut late = client(g.dgk, existing(8));
    connect(&w, &mut late);
    assert_eq!(late.closed, Some(4115), "EXISTING expected for no slot");
    assert_eq!(
        w.mediator().slots(&g.keys.path_public),
        vec![8],
        "the late registration did not come after the close"
    );
    let found = online(&w, &g, existing(8));
    assert!(found.online(), "the second hello did not find the slot");
    let mut other = client(g.dgk, existing(9));
    connect(&w, &mut other);
    assert_eq!(other.closed, Some(4115));
    assert_eq!(
        w.mediator().slots(&g.keys.path_public),
        vec![8],
        "the knob registered a second slot"
    );
    w.violations().assert_empty();
}
