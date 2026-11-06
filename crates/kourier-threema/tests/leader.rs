//! THE LEADER'S PURE PARTS AGAINST THE FAKE (TH6 L3): two implementations meet here. (1) A letter the fake's peer
//! seals with the fake's OWN crypto (`tests/support/mock/peer.rs`: registry X25519, BLAKE2b, XSalsa20-Poly1305) opens
//! with Kourier's open (`src/leader/open.rs`, libthreema's patch-P5 boxes), through the doc-hidden door
//! `store::open_inbound`: every field, a legacy sender with no metadata, padding that is not uniform (T-L4), a legacy
//! nickname that is not UTF-8 (T-L4), and each bent letter refused by the step's rule. (2) The job writers of
//! `src/send/body.rs` equal the fake's builders (`tests/support/mock/body.rs`) byte for byte, through the door
//! `store::write_body`. No socket, no actor: the world's chat server only queues what the peer posts.

use kourier_threema::store::{BodyOf, open_inbound, write_body};

use crate::support::mock::body as fake;
use crate::support::mock::peer::{Letter, LetterMeta};
use crate::support::mock::{MockWorld, START_MS, world};

const ME: &str = "KOURIER1";
const PEER: &str = "PEER0001";
const OTHER: &str = "OTHER001";

/// A world with the user and two peers.
fn three() -> MockWorld {
    let w = world(1301);
    for id in [ME, PEER, OTHER] {
        w.add_identity(id);
    }
    w
}

/// Opens `bytes` as the user with `sender`'s public key, through Kourier's open.
fn open_as_me(
    w: &MockWorld,
    sender: &str,
    bytes: &[u8],
) -> Result<kourier_threema::store::OpenedInbound, &'static str> {
    let me = w.identity(ME).expect("the user");
    let peer = w.identity(sender).expect("the peer");
    open_inbound(me.ck.secret, peer.ck.public, bytes)
}

/// The fake's letter opens with Kourier's open to every field the peer put in it.
#[test]
fn a_letter_the_fakes_peer_seals_opens_with_kouriers_open() {
    let w = three();
    let mut peer = w.peer(PEER);
    let posted = peer.send(
        &Letter::text(ME, 0x0102_0304_0506_0708, "hello there")
            .nickname("  Pat ")
            .flags(0x81)
            .created_ms(START_MS + 4_321),
    );
    let read = open_as_me(&w, PEER, &posted.bytes).expect("Kourier opens the fake's letter");
    assert_eq!(read.sender, PEER);
    assert_eq!(read.receiver, ME);
    assert_eq!(read.message_id, 0x0102_0304_0506_0708);
    assert_eq!(u64::from(read.created_at_s), (START_MS + 4_321) / 1000);
    assert_eq!(read.flags, 0x81);
    let (meta_id, meta_ms, nickname) = read.metadata.clone().expect("a metadata box");
    assert_eq!(
        (meta_id, meta_ms),
        (0x0102_0304_0506_0708, START_MS + 4_321)
    );
    assert_eq!(
        nickname.as_deref(),
        Some("Pat"),
        "trimmed, as libthreema reads it"
    );
    assert_eq!(
        read.legacy_nickname.as_deref(),
        Some(""),
        "zeroes: the empty name"
    );
    assert_eq!(read.outer_type, 0x01);
    assert_eq!(read.body, b"hello there", "the unpadded data");
    assert_eq!(read.body, posted.body, "what the peer posted as the body");
    let shown = format!("{read:?}");
    assert!(
        !shown.contains(PEER) && !shown.contains(ME),
        "no ID in Debug"
    );

    // A group text, a long body (the padding past the 32-byte floor) and a legacy sender with no metadata box.
    let inner = fake::group_member_container(OTHER, 77, &fake::text("hi group"));
    let posted = peer.send(&Letter::new(ME, 2, 0x41, &inner));
    let read = open_as_me(&w, PEER, &posted.bytes).expect("a group text opens");
    assert_eq!(
        (read.outer_type, read.body.as_slice()),
        (0x41, inner.as_slice())
    );
    let long = vec![b'x'; 300];
    let posted = peer.send(&Letter::new(ME, 3, 0x01, &long));
    assert_eq!(
        open_as_me(&w, PEER, &posted.bytes).expect("opens").body,
        long
    );
    let posted = peer.send(&Letter::text(ME, 4, "old").meta(LetterMeta::Absent));
    let read = open_as_me(&w, PEER, &posted.bytes).expect("a legacy letter opens");
    assert!(read.metadata.is_none());
    assert_eq!(read.body, b"old");
    w.violations().assert_empty();
}

/// T-L4: no stricter than the phone: padding bytes that are not uniform open by their last byte alone; a legacy
/// nickname that is not UTF-8 is no nickname and the letter still opens; an unknown type opens (the steps hold it).
#[test]
fn a_letter_kouriers_open_takes_as_libthreema_does() {
    let w = three();
    let mut peer = w.peer(PEER);
    let posted = peer.send(&Letter::text(ME, 5, "hi").padding(&[0xaa, 0xbb, 0xcc, 0x04]));
    let read = open_as_me(&w, PEER, &posted.bytes).expect("non-uniform padding opens");
    assert_eq!(read.body, b"hi");
    let mut legacy = [0_u8; 32];
    legacy[..4].copy_from_slice(&[0xff, 0xfe, 0x41, 0x42]);
    let posted = peer.send(&Letter::text(ME, 6, "hi").legacy(legacy));
    let read = open_as_me(&w, PEER, &posted.bytes).expect("a bad legacy nickname opens");
    assert_eq!(read.legacy_nickname, None);
    let mut named = [0_u8; 32];
    named[..5].copy_from_slice(b" Ann ");
    let posted = peer.send(&Letter::text(ME, 7, "hi").legacy(named));
    assert_eq!(
        open_as_me(&w, PEER, &posted.bytes)
            .expect("opens")
            .legacy_nickname
            .as_deref(),
        Some("Ann")
    );
    let posted = peer.send(&Letter::new(ME, 8, 0x7e, b"future"));
    let read = open_as_me(&w, PEER, &posted.bytes).expect("an unknown type opens");
    assert_eq!(
        (read.outer_type, read.body.as_slice()),
        (0x7e, b"future".as_slice())
    );
    w.violations().assert_empty();
}

/// Each letter the fake bends for a discard of steps 6-8 is refused by that step's rule: a broken box, metadata that
/// is no `MessageMetadata`, the reserved type `0xff`, a zero pad, another sender's key, a payload cut short.
#[test]
fn each_bent_letter_is_refused_by_its_steps_rule() {
    let w = three();
    let mut peer = w.peer(PEER);
    let refused = |posted: &[u8], sender: &str| open_as_me(&w, sender, posted).map(|_| ());
    let broken = peer.send(
        &Letter::text(ME, 10, "x")
            .meta(LetterMeta::Absent)
            .broken_box(),
    );
    assert_eq!(refused(&broken.bytes, PEER), Err("csp-undecryptable"));
    let garbage = peer.send(&Letter::text(ME, 11, "x").meta(LetterMeta::Garbage));
    assert_eq!(refused(&garbage.bytes, PEER), Err("csp-metadata"));
    let ff = peer.send(&Letter::new(ME, 12, 0xff, b"vouch"));
    assert_eq!(refused(&ff.bytes, PEER), Err("csp-legacy-ff"));
    let zero_pad = peer.send(&Letter::text(ME, 13, "x").padding(&[0x00]));
    assert_eq!(refused(&zero_pad.bytes, PEER), Err("csp-container"));
    let fine = peer.send(&Letter::text(ME, 14, "x"));
    assert_eq!(
        refused(&fine.bytes, OTHER),
        Err("csp-metadata"),
        "another sender's key"
    );
    assert_eq!(refused(&fine.bytes[..60], PEER), Err("csp-head"));
    w.violations().assert_empty();
}

/// The job writers of `src/send/body.rs` equal the fake's builders byte for byte: the receipt, the creator container,
/// the setup (and the empty one), the name, the member container, and the empty structs in their containers.
#[test]
fn the_job_writers_equal_the_fakes_builders_byte_for_byte() {
    let ids: [[u8; 8]; 3] = [[1, 2, 3, 4, 5, 6, 7, 8], [0xff; 8], [0; 8]];
    for status in [1_u8, 2, 3, 4] {
        assert_eq!(
            write_body(BodyOf::Receipt { status, ids: &ids }),
            Some(fake::delivery_receipt(status, &ids)),
            "status {status}"
        );
    }
    assert_eq!(
        write_body(BodyOf::Receipt {
            status: 1,
            ids: &ids[..1]
        }),
        Some(fake::delivery_receipt(1, &ids[..1]))
    );
    for members in [&[PEER, OTHER][..], &[], &["*GATEWAY", PEER]] {
        assert_eq!(
            write_body(BodyOf::GroupSetup { members }),
            Some(fake::group_setup(members)),
            "{} members",
            members.len()
        );
    }
    assert_eq!(
        write_body(BodyOf::GroupSetup {
            members: &["short"]
        }),
        None
    );
    for name in ["Rhein", "", "☁ captured", "é\n😀"] {
        assert_eq!(
            write_body(BodyOf::GroupName { name }),
            Some(fake::group_name(name))
        );
    }
    let setup = fake::group_setup(&[PEER]);
    for (group_id, inner) in [
        (0x0102_0304_0506_0708_u64, setup.as_slice()),
        (u64::MAX, &[][..]),
    ] {
        assert_eq!(
            write_body(BodyOf::CreatorContainer { group_id, inner }),
            Some(fake::group_creator_container(group_id, inner))
        );
        assert_eq!(
            write_body(BodyOf::MemberContainer {
                creator: OTHER,
                group_id,
                inner,
            }),
            Some(fake::group_member_container(OTHER, group_id, inner))
        );
    }
    // The creator's answer as the jobs will carry it: setup, name and the empty delete-picture, each in the creator
    // container; the sync request and the leave: the member container around nothing.
    let answer = [
        fake::group_creator_container(9, &fake::group_setup(&[PEER, OTHER])),
        fake::group_creator_container(9, &fake::group_name("mine")),
        fake::group_creator_container(9, &[]),
    ];
    let written = [
        write_body(BodyOf::CreatorContainer {
            group_id: 9,
            inner: &write_body(BodyOf::GroupSetup {
                members: &[PEER, OTHER],
            })
            .expect("IDs"),
        }),
        write_body(BodyOf::CreatorContainer {
            group_id: 9,
            inner: &write_body(BodyOf::GroupName { name: "mine" }).expect("a name"),
        }),
        write_body(BodyOf::CreatorContainer {
            group_id: 9,
            inner: &[],
        }),
    ];
    for (w, f) in written.iter().zip(&answer) {
        assert_eq!(w.as_ref(), Some(f));
    }
    assert_eq!(
        write_body(BodyOf::MemberContainer {
            creator: ME,
            group_id: 9,
            inner: &[],
        }),
        Some(fake::group_member_container(ME, 9, &[]))
    );
}
