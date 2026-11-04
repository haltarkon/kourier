//! The scripted existing device of a join (`support/mock/joiner.rs`, TH4 J2) against libthreema's
//! REAL `RendezvousProtocol::new_as_rid(false, ..)` (Kourier's role) through the fake relay, all in
//! this thread: no runtime, no socket. The QR text is built here in the form Kourier writes (J1's
//! `join::qr`), with the fake's own struct: the fake never imports Kourier's join code. The
//! `EssentialData` builder's identity logs in to the fake mediator through TH2's CSP path.

use libthreema::d2d_rendezvous::{
    RendezvousAuthenticationKey, RendezvousPathStateUpdate, RendezvousProtocol,
};
use libthreema::protobuf::{common, d2d_join, d2d_rendezvous};
use prost::Message as _;

use super::csp::{Setup, connect as csp_connect};
use super::d2m::{Group, SERVER_GROUP, new_device};
use crate::support::mock::crypto::DeviceGroupKeys;
use crate::support::mock::d2m::Out;
use crate::support::mock::essential::{
    Bad, Essential, JOIN_SERVER_GROUP, SHORT_ID, STRANGER_ID, Spec, contact_id, essential,
};
use crate::support::mock::joiner::{
    JoinRequestOrOffer, JoinStep, Joiner, JoinerKnobs, MAX_FRAME_BEFORE_NOMINATION, QR_PREFIX,
    ScanError, Variant, VariantType, decode_qr, frame, url_safe_base64,
};
use crate::support::mock::relay::{Role, SideId};
use crate::support::mock::{MockWorld, world};

const PID: u32 = 1;
const ME: &str = "KOURIER2";

/// The relay URL of a 32-byte rendezvous path, as P11 fills the fake's template.
pub(super) fn relay_url(path: &[u8; 32]) -> String {
    let hex: String = path.iter().map(|b| format!("{b:02x}")).collect();
    format!("wss://rendezvous-{}.invalid/{}/{hex}", &hex[..1], &hex[..2])
}

/// A join QR text in the form Kourier writes (`src/join/qr.rs`): `V1_0`, `request_to_join`,
/// D2D protocol V0_3, one relayed path with PID 1 and network cost UNKNOWN, URL-safe base64 WITH
/// padding.
pub(super) fn qr_text(ak: &[u8; 32], url: &str, variant: VariantType) -> String {
    let offer = JoinRequestOrOffer {
        version: 0,
        variant: Some(Variant {
            r#type: Some(variant),
        }),
        rendezvous_init: Some(d2d_rendezvous::RendezvousInit {
            version: 0,
            ak: ak.to_vec(),
            relayed_web_socket: Some(d2d_rendezvous::rendezvous_init::RelayedWebSocket {
                path_id: PID,
                network_cost: 0,
                url: url.to_owned(),
            }),
            direct_tcp_server: None,
        }),
        d2d_protocol_version: 3,
    };
    format!("{QR_PREFIX}{}", url_safe_base64(&offer.encode_to_vec()))
}

pub(super) fn request() -> VariantType {
    VariantType::RequestToJoin(common::Unit {})
}

/// libthreema's RID on its relay side, and what it saw.
struct Rid {
    proto: RendezvousProtocol,
    side: SideId,
    awaiting_nominate: bool,
    rph: Option<[u8; 32]>,
    ulp: Vec<Vec<u8>>,
    failed: bool,
    closed: Option<u16>,
}

/// A device group of the world, its data, a joiner that hands it over, and libthreema's RID on a
/// fresh relay path with the QR the joiner scans (not scanned yet).
struct Scene {
    w: MockWorld,
    data: Essential,
    joiner: Joiner,
    rid: Rid,
    qr: String,
}

fn scene(seed: u64, spec: &Spec, knobs: JoinerKnobs, nominator: bool) -> Scene {
    let w = world(seed);
    let group = w.device_group(ME);
    let data = essential(&w, &group, spec);
    let joiner = w.joiner(data.clone(), knobs);
    let (ak, path): ([u8; 32], [u8; 32]) = (w.random(), w.random());
    let url = relay_url(&path);
    let side = w.relay().connect(&url, Role::Rid);
    let rid = Rid {
        proto: RendezvousProtocol::new_as_rid(nominator, RendezvousAuthenticationKey(ak), &[PID]),
        side,
        awaiting_nominate: false,
        rph: None,
        ulp: Vec::new(),
        failed: false,
        closed: None,
    };
    let qr = qr_text(&ak, &url, request());
    Scene {
        w,
        data,
        joiner,
        rid,
        qr,
    }
}

/// libthreema's frame as ONE relay message (`header || payload`, Kourier's form, brief §2.10).
fn send(s: &Scene, bytes: Vec<u8>) {
    s.w.relay().receive(s.rid.side, bytes);
}

/// Moves frames between libthreema's RID, the relay and the joiner until nothing moves.
fn run(s: &mut Scene) {
    for _ in 0..1_000 {
        let mut moved = s.joiner.pump();
        for out in s.w.relay().take(s.rid.side) {
            moved = true;
            match out {
                Out::Datagram(d) => {
                    if s.rid.proto.add_chunks(PID, &[&d]).is_err() {
                        s.rid.failed = true;
                    }
                }
                Out::Close(code) => s.rid.closed = Some(code),
            }
        }
        while !s.rid.failed && s.rid.closed.is_none() {
            match s.rid.proto.process_frame(PID) {
                Ok(Some(result)) => {
                    match result.state_update {
                        Some(RendezvousPathStateUpdate::Nominated { rph }) => {
                            s.rid.rph = Some(rph.0);
                        }
                        Some(RendezvousPathStateUpdate::AwaitingNominate { .. }) => {
                            s.rid.awaiting_nominate = true;
                        }
                        None => {}
                    }
                    if let Some(data) = result.incoming_ulp_data {
                        s.rid.ulp.push(data);
                    }
                    if let Some(f) = result.outgoing_frame {
                        send(s, f.into());
                        moved = true;
                    }
                }
                Ok(None) => break,
                Err(_) => s.rid.failed = true,
            }
        }
        if !moved {
            return;
        }
    }
    panic!("the rendezvous did not settle");
}

/// What libthreema's RID got as `EdToNd`, by name.
fn ed_to_nd(rid: &Rid) -> Vec<&'static str> {
    use d2d_join::ed_to_nd::Content;
    rid.ulp
        .iter()
        .map(
            |p| match d2d_join::EdToNd::decode(p.as_slice()).map(|m| m.content) {
                Ok(Some(Content::Begin(_))) => "begin",
                Ok(Some(Content::BlobData(_))) => "blob-data",
                Ok(Some(Content::EssentialData(_))) => "essential-data",
                _ => "other",
            },
        )
        .collect()
}

fn registered() -> Vec<u8> {
    d2d_join::NdToEd {
        content: Some(d2d_join::nd_to_ed::Content::Registered(
            d2d_join::Registered {},
        )),
    }
    .encode_to_vec()
}

/// The handshake reaches `Nominated` on BOTH sides (the joiner nominates, libthreema is no
/// nominator) and both path hashes are equal: the fake's own RRDAK/RIDAK, STK, RRDTK/RIDTK and
/// RPH equal libthreema's.
#[test]
fn the_handshake_reaches_nominated_on_both_sides_with_one_path_hash() {
    let knobs = JoinerKnobs {
        withhold_begin: true,
        ..Default::default()
    };
    let mut s = scene(501, &Spec::default(), knobs, false);
    s.joiner.scan(&s.qr).expect("the joiner read Kourier's QR");
    run(&mut s);
    assert!(!s.rid.failed, "libthreema's RID failed");
    assert!(s.rid.awaiting_nominate, "the RID never awaited nomination");
    assert_eq!(s.joiner.step(), JoinStep::Nominated, "the joiner's step");
    assert_eq!(s.rid.proto.nominated_path(), Some(PID), "not nominated");
    let (ours, theirs) = (s.joiner.rph(), s.rid.rph);
    assert!(ours.is_some() && theirs.is_some(), "a path hash is missing");
    assert!(ours == theirs, "the two path hashes differ");
    assert_eq!(
        s.joiner.sent(),
        vec![("hello", 1), ("auth", 1), ("nominate", 1)],
        "the joiner's frames"
    );
    s.w.violations().assert_empty();
}

/// The knob `nominate_after` waits on the virtual clock, and not less.
#[test]
fn the_nomination_waits_for_its_knob() {
    let knobs = JoinerKnobs {
        nominate_after: std::time::Duration::from_secs(3),
        withhold_begin: true,
        ..Default::default()
    };
    let mut s = scene(502, &Spec::default(), knobs, false);
    s.joiner.scan(&s.qr).expect("scanned");
    run(&mut s);
    assert_eq!(
        s.joiner.step(),
        JoinStep::Authenticated,
        "nominated at once"
    );
    s.w.advance(std::time::Duration::from_millis(2_999));
    run(&mut s);
    assert_eq!(s.joiner.step(), JoinStep::Authenticated, "nominated early");
    s.w.advance(std::time::Duration::from_millis(1));
    run(&mut s);
    assert_eq!(s.joiner.step(), JoinStep::Nominated, "not nominated at 3 s");
    assert!(s.rid.rph.is_some(), "the RID was not told");
    s.w.violations().assert_empty();
}

/// A joiner on another AK than the QR's: libthreema's RID cannot open its Hello and stops; when
/// the RID closes with 4000 (Kourier's protocol error) the joiner hears 4004 from the relay.
#[test]
fn a_wrong_ak_fails_the_handshake() {
    let knobs = JoinerKnobs {
        wrong_ak: true,
        ..Default::default()
    };
    let mut s = scene(503, &Spec::default(), knobs, false);
    s.joiner.scan(&s.qr).expect("scanned");
    run(&mut s);
    assert!(s.rid.failed, "libthreema took a Hello under another AK");
    s.w.relay().client_close(s.rid.side, 4000);
    run(&mut s);
    assert_eq!(s.joiner.closed(), Some(4004), "the joiner's end");
    assert!(s.joiner.rph().is_none(), "a path hash without a handshake");
    s.w.violations().assert_empty();
}

/// An AuthHello that does not open under RIDAK (what a RID with another AK sends) is closed by the
/// joiner with 4000 and recorded; the RID hears 4004.
#[test]
fn an_auth_hello_that_does_not_open_is_closed_with_4000_and_recorded() {
    let mut s = scene(504, &Spec::default(), JoinerKnobs::default(), false);
    s.joiner.scan(&s.qr).expect("scanned");
    let hello = s.w.relay().take(s.rid.side);
    let [Out::Datagram(hello)] = hello.as_slice() else {
        panic!("the RID did not get one Hello");
    };
    s.rid.proto.add_chunks(PID, &[hello]).expect("chunks");
    let result = s
        .rid
        .proto
        .process_frame(PID)
        .expect("libthreema took the Hello")
        .expect("a frame");
    let mut auth_hello: Vec<u8> = result.outgoing_frame.expect("an AuthHello").into();
    let last = auth_hello.len() - 1;
    auth_hello[last] ^= 0x01;
    send(&s, auth_hello);
    s.joiner.pump();
    assert_eq!(s.joiner.closed(), Some(4000), "the joiner's close");
    assert_eq!(
        s.w.relay().take(s.rid.side),
        vec![Out::Close(4004)],
        "the RID's end"
    );
    assert_eq!(
        s.w.violations().rules(),
        vec!["rendezvous-auth-hello-refused"],
        "the client's breach"
    );
}

/// ULP frames both ways after nomination: the joiner's `Begin`, `BlobData` and `EssentialData`
/// open in libthreema (the builder's bytes exactly), libthreema's `Registered` opens in the joiner,
/// and one more frame each way after that.
#[test]
fn ulp_frames_go_both_ways_after_nomination() {
    let spec = Spec {
        contacts: 3,
        groups: 2,
        hashed_nonces: 4,
        profile_picture: true,
        bad: Vec::new(),
    };
    let mut s = scene(505, &spec, JoinerKnobs::default(), false);
    s.joiner.scan(&s.qr).expect("scanned");
    run(&mut s);
    assert!(!s.rid.failed, "libthreema's RID failed");
    assert_eq!(
        ed_to_nd(&s.rid),
        vec!["begin", "blob-data", "essential-data"],
        "what the new device got"
    );
    let got = d2d_join::EdToNd::decode(s.rid.ulp[2].as_slice()).expect("an EdToNd");
    let Some(d2d_join::ed_to_nd::Content::EssentialData(data)) = got.content else {
        panic!("no EssentialData");
    };
    assert!(data == s.data.data, "the EssentialData changed on the way");
    assert_eq!(
        s.joiner.step(),
        JoinStep::EssentialSent,
        "the joiner's step"
    );
    let frame = s
        .rid
        .proto
        .create_ulp_frame(registered())
        .expect("libthreema's ULP frame")
        .outgoing_frame
        .expect("a frame");
    send(&s, frame.into());
    run(&mut s);
    assert_eq!(s.joiner.step(), JoinStep::Registered, "Registered not read");
    assert!(
        s.joiner.ulp_received() == vec![registered()],
        "the joiner opened other bytes"
    );
    assert!(
        s.joiner.send_ulp(b"after registered"),
        "the joiner sent nothing"
    );
    run(&mut s);
    assert!(
        s.rid.ulp.last().is_some_and(|p| p == b"after registered"),
        "libthreema opened other bytes"
    );
    s.w.violations().assert_empty();
}

/// A nominating RID (libthreema as nominator, the joiner told never to nominate): the joiner
/// records the nomination by the RID and closes with 4000.
#[test]
fn a_nomination_by_the_rid_is_recorded() {
    let knobs = JoinerKnobs {
        never_nominate: true,
        ..Default::default()
    };
    let mut s = scene(506, &Spec::default(), knobs, true);
    s.joiner.scan(&s.qr).expect("scanned");
    run(&mut s);
    assert!(
        s.rid.awaiting_nominate,
        "the RID did not finish the handshake"
    );
    let nominate = s
        .rid
        .proto
        .nominate_path(PID)
        .expect("libthreema nominates")
        .outgoing_frame
        .expect("a Nominate");
    send(&s, nominate.into());
    run(&mut s);
    assert_eq!(s.joiner.closed(), Some(4000), "the joiner's close");
    assert_eq!(s.rid.closed, Some(4004), "the RID's end");
    assert_eq!(
        s.w.violations().rules(),
        vec!["rendezvous-rid-nominated"],
        "the client's breach"
    );
}

/// A frame of the RID while the path waits for the joiner's nomination is an ULP frame too early.
#[test]
fn an_ulp_frame_before_nomination_is_recorded() {
    let knobs = JoinerKnobs {
        never_nominate: true,
        ..Default::default()
    };
    let mut s = scene(507, &Spec::default(), knobs, false);
    s.joiner.scan(&s.qr).expect("scanned");
    run(&mut s);
    assert_eq!(
        s.joiner.step(),
        JoinStep::Authenticated,
        "the joiner's step"
    );
    send(&s, frame(&[0x11; 40]));
    run(&mut s);
    assert_eq!(s.joiner.closed(), Some(4000), "the joiner's close");
    assert_eq!(
        s.w.violations().rules(),
        vec!["rendezvous-ulp-before-nomination"],
        "the client's breach"
    );
}

/// The knobs of the blobs: a `BlobData` cut over three messages reassembles in libthreema, a flood
/// of unreferenced blobs comes first, `skip_blob` leaves the referenced one out, `essential_twice`
/// sends the data twice.
#[test]
fn the_blob_knobs_shape_what_the_new_device_gets() {
    let spec = Spec {
        profile_picture: true,
        ..Default::default()
    };
    let knobs = JoinerKnobs {
        blob_split: Some(3),
        blob_flood: Some((2, 1_000)),
        ..Default::default()
    };
    let mut s = scene(508, &spec, knobs, false);
    s.joiner.scan(&s.qr).expect("scanned");
    run(&mut s);
    assert_eq!(
        ed_to_nd(&s.rid),
        vec![
            "begin",
            "blob-data",
            "blob-data",
            "blob-data",
            "essential-data"
        ],
        "flood, then the referenced blob"
    );
    let cuts: Vec<usize> = s
        .joiner
        .sent()
        .iter()
        .filter(|(name, _)| *name == "blob-data")
        .map(|(_, n)| *n)
        .collect();
    assert_eq!(cuts, vec![3, 3, 3], "each BlobData over three messages");
    let knobs = JoinerKnobs {
        skip_blob: true,
        essential_twice: true,
        ..Default::default()
    };
    let mut t = scene(509, &spec, knobs, false);
    t.joiner.scan(&t.qr).expect("scanned");
    run(&mut t);
    assert_eq!(
        ed_to_nd(&t.rid),
        vec!["begin", "essential-data", "essential-data"],
        "the blob skipped, the data twice"
    );
    s.w.violations().assert_empty();
    t.w.violations().assert_empty();
}

/// The joiner reads the QR text in Kourier's form (padding kept); an unpadded text, an offer to
/// join and a foreign prefix are refused with their own rules.
#[test]
fn the_joiner_reads_kouriers_qr_and_refuses_others() {
    let w = world(510);
    let ak: [u8; 32] = core::array::from_fn(|i| u8::try_from(i * 7 % 256).expect("a byte"));
    let path: [u8; 32] = core::array::from_fn(|i| u8::try_from(255 - i).expect("a byte"));
    let url = relay_url(&path);
    let text = qr_text(&ak, &url, request());
    let offer = decode_qr(&text).expect("Kourier's QR");
    let init = offer.rendezvous_init.expect("an init");
    assert!(init.ak == ak, "another AK");
    let relayed = init.relayed_web_socket.expect("a relayed path");
    assert_eq!(relayed.path_id, PID, "the path id");
    assert!(relayed.url == url, "another URL");
    assert_eq!(offer.d2d_protocol_version, 3, "V0_3");
    let b64 = text.strip_prefix(QR_PREFIX).expect("the prefix");
    assert!(b64.ends_with('='), "this payload's base64 is padded");
    let joiner = w.joiner(
        essential(&w, &w.device_group(ME), &Spec::default()),
        JoinerKnobs::default(),
    );
    let unpadded = text.trim_end_matches('=');
    let offer_to_join = qr_text(&ak, &url, VariantType::OfferToJoin(common::Unit {}));
    for bad in [
        unpadded.to_owned(),
        offer_to_join,
        text.replacen("threema://", "threema:/", 1),
    ] {
        assert!(
            matches!(joiner.scan(&bad), Err(ScanError::Unreadable(_))),
            "a QR the phone refuses was read"
        );
    }
    assert_eq!(
        w.violations().rules(),
        vec!["join-qr-base64", "join-qr-variant", "join-qr-prefix"],
        "the refusals' rules"
    );
}

/// P10: the phone refuses a QR with a direct TCP server (Kourier makes none) and one whose path ids
/// are not unique (`TP/md-d2d-rendezvous.proto` "If any `path_id` is not unique, abort"), each with its
/// own rule; Kourier's own form passes.
#[test]
fn the_joiner_refuses_a_qr_with_a_direct_tcp_server_or_a_path_id_twice() {
    use d2d_rendezvous::rendezvous_init::DirectTcpServer;
    use d2d_rendezvous::rendezvous_init::direct_tcp_server::IpAddress;
    let w = world(512);
    let ak: [u8; 32] = core::array::from_fn(|i| u8::try_from(i * 5 % 256).expect("a byte"));
    let path: [u8; 32] = core::array::from_fn(|i| u8::try_from(200 - i).expect("a byte"));
    let url = relay_url(&path);
    let with_direct = |ids: &[u32]| {
        let mut offer = decode_qr(&qr_text(&ak, &url, request())).expect("Kourier's QR");
        let init = offer.rendezvous_init.as_mut().expect("an init");
        init.direct_tcp_server = Some(DirectTcpServer {
            port: 4_000,
            ip_addresses: ids
                .iter()
                .map(|id| IpAddress {
                    path_id: *id,
                    network_cost: 0,
                    ip: "192.0.2.1".to_owned(),
                })
                .collect(),
        });
        format!("{QR_PREFIX}{}", url_safe_base64(&offer.encode_to_vec()))
    };
    let joiner = w.joiner(
        essential(&w, &w.device_group(ME), &Spec::default()),
        JoinerKnobs::default(),
    );
    for text in [
        with_direct(&[PID + 1]),
        with_direct(&[PID]),
        with_direct(&[PID + 1, PID + 1]),
    ] {
        assert!(
            matches!(joiner.scan(&text), Err(ScanError::Unreadable(_))),
            "a QR the phone refuses was read"
        );
    }
    assert!(
        decode_qr(&qr_text(&ak, &url, request())).is_ok(),
        "Kourier's own form was refused"
    );
    assert_eq!(
        w.violations().rules(),
        vec![
            "join-qr-direct-tcp",
            "join-qr-path-id-not-unique",
            "join-qr-path-id-not-unique"
        ],
        "the refusals' rules"
    );
}

/// P10: a frame of the RID over 16 KiB before the nomination is a violation (judged at the header, as
/// libthreema's decoder judges it) and the joiner closes 4000, in both steps before its nomination;
/// exactly 16 KiB is not one.
#[test]
fn a_frame_over_16_kib_before_the_nomination_is_a_violation() {
    let limit = u32::try_from(MAX_FRAME_BEFORE_NOMINATION).expect("16 KiB");
    // After its Hello, before the RID's AuthHello.
    let s = scene(513, &Spec::default(), JoinerKnobs::default(), false);
    s.joiner.scan(&s.qr).expect("scanned");
    send(&s, limit.to_le_bytes().to_vec());
    s.joiner.pump();
    assert_eq!(s.joiner.closed(), None, "16 KiB exactly was refused");
    assert!(s.w.violations().rules().is_empty(), "16 KiB exactly");
    let s = scene(514, &Spec::default(), JoinerKnobs::default(), false);
    s.joiner.scan(&s.qr).expect("scanned");
    send(&s, (limit + 1).to_le_bytes().to_vec());
    s.joiner.pump();
    assert_eq!(s.joiner.closed(), Some(4000), "not closed 4000 (scanned)");
    assert_eq!(
        s.w.violations().rules(),
        vec!["rendezvous-oversized-frame"],
        "the rule (scanned)"
    );
    // After the handshake, before nomination (the phone never nominates).
    let knobs = JoinerKnobs {
        never_nominate: true,
        ..Default::default()
    };
    let mut s = scene(515, &Spec::default(), knobs, false);
    s.joiner.scan(&s.qr).expect("scanned");
    run(&mut s);
    assert_eq!(
        s.joiner.step(),
        JoinStep::Authenticated,
        "the joiner's step"
    );
    send(&s, (limit + 1).to_le_bytes().to_vec());
    s.joiner.pump();
    assert_eq!(
        s.joiner.closed(),
        Some(4000),
        "not closed 4000 (authenticated)"
    );
    assert_eq!(
        s.w.violations().rules(),
        vec!["rendezvous-oversized-frame"],
        "the rule (authenticated)"
    );
}

/// The builder: the identity, CK, DGK, cookie and server group are the world's device group's;
/// the counts are the spec's; IDs are 8 characters and a substring of no other value; each bad
/// item is there.
#[test]
fn the_builder_makes_the_device_groups_data_and_its_bad_items() {
    let w = world(511);
    let group = w.device_group(ME);
    let spec = Spec {
        contacts: 12,
        groups: 3,
        hashed_nonces: 5,
        profile_picture: true,
        bad: Vec::new(),
    };
    let e = essential(&w, &group, &spec);
    let id = e.data.identity_data.as_ref().expect("identity data");
    assert!(id.identity == group.identity.id, "another identity");
    assert!(id.ck == group.identity.ck.secret, "another CK");
    assert!(id.csp_device_cookie == group.cookie, "another cookie");
    assert_eq!(id.csp_server_group, JOIN_SERVER_GROUP, "the server group");
    let dgk = e.data.device_group_data.as_ref().expect("a DGK");
    assert!(dgk.dgk == group.dgk, "another DGK");
    assert_eq!(e.data.contacts.len(), 12, "contacts");
    assert_eq!(e.data.groups.len(), 3, "groups");
    assert_eq!(e.data.csp_hashed_nonces.len(), 5, "CSP nonces");
    assert_eq!(e.data.d2d_hashed_nonces.len(), 5, "D2D nonces");
    assert_eq!(e.blobs.len(), 1, "the profile picture's blob");
    assert!(e.data.work_credentials.is_none(), "work credentials");
    // Every synthetic string value, and every ID among them.
    let mut values: Vec<String> = Vec::new();
    for c in e.data.contacts.iter().filter_map(|c| c.contact.as_ref()) {
        values.push(c.identity.clone());
        values.extend(c.first_name.clone());
        values.extend(c.last_name.clone());
    }
    for g in e.data.groups.iter().filter_map(|g| g.group.as_ref()) {
        values.extend(g.name.clone());
        values.extend(
            g.member_identities
                .iter()
                .flat_map(|m| m.identities.clone()),
        );
    }
    let ids: Vec<String> = (0..12).map(contact_id).collect();
    for i in &ids {
        assert_eq!(i.len(), 8, "a synthetic ID of another length");
        for v in values.iter().filter(|v| *v != i) {
            assert!(
                !v.contains(i.as_str()),
                "a synthetic ID is inside another value"
            );
        }
    }
    for bad in [
        Bad::ShortContactId,
        Bad::DuplicateContact,
        Bad::MemberNotAContact,
        Bad::OwnIdAsContact,
        Bad::WorkCredentials,
    ] {
        let one = Spec {
            contacts: 2,
            groups: 1,
            bad: vec![bad],
            ..Default::default()
        };
        let e = essential(&w, &group, &one);
        let contacts: Vec<&str> = e
            .data
            .contacts
            .iter()
            .filter_map(|c| c.contact.as_ref().map(|c| c.identity.as_str()))
            .collect();
        let members: Vec<&str> = e
            .data
            .groups
            .iter()
            .filter_map(|g| g.group.as_ref())
            .flat_map(|g| g.member_identities.iter().flat_map(|m| m.identities.iter()))
            .map(String::as_str)
            .collect();
        let there = match bad {
            Bad::ShortContactId => contacts.contains(&SHORT_ID) && SHORT_ID.len() == 7,
            Bad::DuplicateContact => contacts.len() == 3 && contacts[0] == contacts[2],
            Bad::MemberNotAContact => members.contains(&STRANGER_ID),
            Bad::OwnIdAsContact => contacts.contains(&ME),
            Bad::WorkCredentials => e.data.work_credentials.is_some(),
        };
        assert!(there, "a bad item is missing: {bad:?}");
    }
    w.violations().assert_empty();
}

/// The builder's identity logs in to the fake mediator and chat server through TH2's path: the
/// real D2M client on the data's DGK, the real CSP client with its ID, CK and cookie.
#[test]
fn the_builders_identity_logs_in_to_the_fake_mediator() {
    let w = world(512);
    let group = w.device_group(ME);
    let e = essential(&w, &group, &Spec::default());
    let id = e.data.identity_data.as_ref().expect("identity data");
    assert_eq!(
        id.csp_server_group, SERVER_GROUP,
        "TH2's client dials this server group"
    );
    let dgk: [u8; 32] = e
        .data
        .device_group_data
        .as_ref()
        .expect("a DGK")
        .dgk
        .as_slice()
        .try_into()
        .expect("32 bytes");
    let g = Group {
        dgk,
        keys: DeviceGroupKeys::derive(&dgk),
    };
    let setup = Setup {
        identity: id.identity.clone(),
        ck_secret: id.ck.as_slice().try_into().expect("a 32-byte CK"),
        server_keys: vec![w.server_key().public],
        cookie: Some(
            id.csp_device_cookie
                .as_slice()
                .try_into()
                .expect("16 bytes"),
        ),
        csp_device_id: Some(0x0bad_cafe),
        split_hello: false,
    };
    let d = csp_connect(&w, &g, new_device(0x1234), &setup);
    assert!(d.logged_in(), "the joined identity did not log in");
    w.violations().assert_empty();
}
