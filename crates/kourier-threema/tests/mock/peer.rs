//! The fake's CSP peer (`support/mock/peer.rs`, TH6a S3). Nothing in Kourier sends yet, so these
//! tests SEAL THEIR OWN BOXES with the fake's crypto, from the spec's layout (`Wire` below), and
//! hand them to the peer as the chat server would keep them. No client, no runtime, no `unshare`.
//! The tests of the knobs that need a client on the wire are in `csp.rs`, `reflect.rs` and
//! `connector.rs` and use `Wire` from here. Every test ends with the violations check; the
//! refusals name the rule they expect.

use libthreema::protobuf::csp_e2e;
use prost::Message as _;

use crate::support::mock::crypto::{blake2b_mac, hsalsa_shared, seal};
use crate::support::mock::csp::{Sent, flag};
use crate::support::mock::peer::{Discard, HEAD, LEGACY_NICKNAME, metadata_key, unpad};
use crate::support::mock::{START_MS, TestIdentity, world};

pub(super) const KOURIER: &str = "KOURIER1";
pub(super) const PEER: &str = "PEER0001";
const GATEWAY: &str = "*GATEWAY";

/// What the metadata box of a built message is.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Meta {
    /// Sealed under the MMK, a protobuf with the fields the spec names.
    Normal,
    /// `metadata-length` 0 and no box.
    Absent,
    /// Sealed under the message key instead of the MMK.
    WrongKey,
    /// Sealed under the MMK, but the plaintext is no protobuf.
    Garbage,
}

/// A `message-with-metadata-box` written from `TP/csp.struct.yml`, field by field, and sealed with
/// the fake's crypto. The fields are what a test bends.
pub(super) struct Wire<'a> {
    pub(super) from: &'a TestIdentity,
    pub(super) to: &'a TestIdentity,
    pub(super) id: u64,
    pub(super) created_ms: u64,
    /// The head's `created-at` in seconds when it is not the metadata's.
    pub(super) head_s: Option<u32>,
    pub(super) flags: u8,
    pub(super) ty: u8,
    /// The container's data before the padding.
    pub(super) data: Vec<u8>,
    /// The padding bytes when they are not the PKCS#7 padding to 32 bytes.
    pub(super) padding: Option<Vec<u8>>,
    pub(super) nickname: Option<String>,
    /// The metadata's message id when it is not the head's.
    pub(super) meta_id: Option<u64>,
    /// The head's legacy nickname when it is not the rule's.
    pub(super) legacy: Option<[u8; LEGACY_NICKNAME]>,
    pub(super) nonce: [u8; 24],
    pub(super) meta: Meta,
    pub(super) conn: u64,
}

impl<'a> Wire<'a> {
    /// A text message (type 0x01, "text") from `from` to `to`, with the nonce `[nonce; 24]`.
    pub(super) fn new(from: &'a TestIdentity, to: &'a TestIdentity, id: u64, nonce: u8) -> Self {
        Self {
            from,
            to,
            id,
            created_ms: START_MS + 500,
            head_s: None,
            flags: flag::SEND_PUSH,
            ty: 0x01,
            data: b"text".to_vec(),
            padding: None,
            nickname: None,
            meta_id: None,
            legacy: None,
            nonce: [nonce; 24],
            meta: Meta::Normal,
            conn: 1,
        }
    }

    /// The struct as the chat server keeps it.
    pub(super) fn sent(&self) -> Sent {
        // S = X25519HSalsa20(<sender.CK>.secret, <receiver.CK>.public)
        let shared = hsalsa_shared(&self.from.ck.secret, &self.to.ck.public).expect("contributory");
        // The container: type, then the data and its padding (PKCS#7, up to 32 bytes in all).
        let mut container = vec![self.ty];
        container.extend_from_slice(&self.data);
        match &self.padding {
            Some(padding) => container.extend_from_slice(padding),
            None => {
                let pad = 32usize.saturating_sub(self.data.len()).max(1);
                container.extend(std::iter::repeat_n(
                    u8::try_from(pad).expect("a pad of at most 32"),
                    pad,
                ));
            }
        }
        let message_box = seal(&shared, &self.nonce, &container);
        let metadata = csp_e2e::MessageMetadata {
            #[expect(
                deprecated,
                reason = "the spec's padding field: 16 bytes, ignored by the receiver"
            )]
            padding: vec![0; 16],
            message_id: self.meta_id.unwrap_or(self.id),
            created_at: self.created_ms,
            nickname: self.nickname.clone(),
        }
        .encode_to_vec();
        let metadata_box = match self.meta {
            Meta::Absent => Vec::new(),
            Meta::WrongKey => seal(&shared, &self.nonce, &metadata),
            Meta::Normal => seal(&metadata_key(&shared), &self.nonce, &metadata),
            Meta::Garbage => seal(&metadata_key(&shared), &self.nonce, &[0x0a, 0xff]),
        };
        let head_s = self
            .head_s
            .unwrap_or_else(|| u32::try_from(self.created_ms / 1000).expect("seconds fit u32"));
        let legacy = self.legacy.unwrap_or_else(|| {
            let mut field = [0u8; LEGACY_NICKNAME];
            if self.to.id.starts_with('*') {
                let nick = self.nickname.as_deref().unwrap_or("").as_bytes();
                field[..nick.len().min(LEGACY_NICKNAME)]
                    .copy_from_slice(&nick[..nick.len().min(LEGACY_NICKNAME)]);
            }
            field
        });
        let mut bytes = Vec::new();
        bytes.extend_from_slice(self.from.id.as_bytes());
        bytes.extend_from_slice(self.to.id.as_bytes());
        bytes.extend_from_slice(&self.id.to_le_bytes());
        bytes.extend_from_slice(&head_s.to_le_bytes());
        bytes.push(self.flags);
        bytes.push(0);
        bytes.extend_from_slice(
            &u16::try_from(metadata_box.len())
                .expect("a metadata box fits u16")
                .to_le_bytes(),
        );
        bytes.extend_from_slice(&legacy);
        assert_eq!(bytes.len(), HEAD, "the head is 64 bytes");
        bytes.extend_from_slice(&metadata_box);
        bytes.extend_from_slice(&self.nonce);
        bytes.extend_from_slice(&message_box);
        Sent {
            sender: self.from.id.as_bytes().try_into().expect("8 bytes"),
            receiver: self.to.id.as_bytes().try_into().expect("8 bytes"),
            message_id: self.id,
            created_at: head_s,
            flags: self.flags,
            bytes,
            conn: self.conn,
            replaced: false,
        }
    }
}

/// The spec's MMK for `S` = 32 bytes of 0x11 and for `S` = 0, 1, .. 31, computed OUTSIDE this
/// crate (Python's `hashlib.blake2b(b'', key=S, salt=b'mm', person=b'3ma-csp', digest_size=32)`).
#[test]
fn the_metadata_key_has_the_specs_known_answers() {
    let want = |hex: &str| super::unhex32(hex);
    assert_eq!(
        metadata_key(&[0x11; 32]),
        want("c38487c0327a7a63a9e786d09eb99e6ab532acc7c741fbfbbee8bd8fe3a99a65"),
        "S of 0x11 bytes"
    );
    let counting: [u8; 32] = std::array::from_fn(|i| u8::try_from(i).expect("below 32"));
    assert_eq!(
        metadata_key(&counting),
        want("c5bafee562420f666d519b53b0770fdef2d136c9095bd0179f5ed5e2a254bd0f"),
        "S of 0, 1, .. 31"
    );
    // The same key through the fake's own BLAKE2b helper with the spec's parameters.
    assert_eq!(
        metadata_key(&[0x11; 32]),
        blake2b_mac(&[0x11; 32], b"mm", b"3ma-csp", &[]),
        "the helper and the named key agree"
    );
    let w = world(401);
    w.violations().assert_empty();
}

/// The peer opens a box the fake sealed itself: every field of the head by its offset, the metadata
/// under the MMK, the container's type and the unpadded data; the DH is the same from both ends.
#[test]
fn the_peer_opens_a_box_the_fake_seals_itself() {
    let w = world(402);
    let kourier = w.add_identity(KOURIER);
    let peer_identity = w.add_identity(PEER);
    let mut peer = w.peer(PEER);
    let mut wire = Wire::new(&kourier, &peer_identity, 0x1122_3344_5566_7788, 7);
    wire.created_ms = START_MS + 1_234;
    wire.flags = flag::SEND_PUSH | flag::NO_SERVER_QUEUING;
    wire.data = b"hello".to_vec();
    wire.nickname = Some("Kourier Test".to_owned());
    let sent = wire.sent();
    // The head by the spec's offsets, read here with literals.
    assert_eq!(&sent.bytes[..8], b"KOURIER1", "sender");
    assert_eq!(&sent.bytes[8..16], b"PEER0001", "receiver");
    assert_eq!(
        sent.bytes[16..24],
        [0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11],
        "message id, little endian"
    );
    assert_eq!(sent.bytes[28], 0x03, "flags");
    assert_eq!(sent.bytes[29], 0, "reserved");
    assert_eq!(
        sent.bytes[32..64],
        [0; 32],
        "legacy nickname, not a gateway"
    );
    let opened = peer.open_sent(&sent, 0).expect("the peer opens the box");
    assert_eq!(opened.sender, KOURIER);
    assert_eq!(opened.message_id, 0x1122_3344_5566_7788);
    assert_eq!(opened.created_at_ms, START_MS + 1_234);
    assert_eq!(
        u64::from(opened.created_at_s),
        (START_MS + 1_234) / 1000,
        "the head's seconds"
    );
    assert_eq!(opened.flags, 0x03);
    assert_eq!(opened.ty, 0x01);
    assert_eq!(opened.body, b"hello".to_vec(), "the unpadded data");
    assert_eq!(opened.nickname.as_deref(), Some("Kourier Test"));
    assert_eq!(opened.nonce, [7; 24]);
    assert_eq!(opened.discard, None);
    assert_eq!(opened.bytes, sent.bytes);
    assert_eq!(peer.opened().len(), 1);
    // An empty nickname is "cleared", an absent one is not the same thing.
    let mut cleared = Wire::new(&kourier, &peer_identity, 2, 8);
    cleared.nickname = Some(String::new());
    let again = peer.open_sent(&cleared.sent(), 1).expect("opens");
    assert_eq!(again.nickname.as_deref(), Some(""), "a cleared nickname");
    let absent = Wire::new(&kourier, &peer_identity, 3, 9);
    let third = peer.open_sent(&absent.sent(), 2).expect("opens");
    assert_eq!(third.nickname, None, "no nickname field");
    w.violations().assert_empty();
}

/// The legacy nickname: all zeroes for a peer that is no Gateway ID, the metadata's nickname for a
/// Gateway ID, cut on a character at 32 bytes when it is longer.
#[test]
fn the_legacy_nickname_is_zero_but_for_a_gateway_id() {
    let w = world(403);
    let kourier = w.add_identity(KOURIER);
    let person = w.add_identity(PEER);
    let gateway = w.add_identity(GATEWAY);
    let mut to_person = w.peer(PEER);
    let mut to_gateway = w.peer(GATEWAY);
    let mut expected: Vec<&str> = Vec::new();
    let mut n = 0u8;
    let mut next = || {
        n += 1;
        n
    };

    // A Gateway ID with the nickname written out (padded with zeroes): fine.
    let mut a = Wire::new(&kourier, &gateway, 1, next());
    a.nickname = Some("Kourier".to_owned());
    assert!(
        to_gateway.open_sent(&a.sent(), 0).is_some(),
        "the nickname for a gateway"
    );
    // A Gateway ID, no nickname in the metadata: all zeroes.
    let b = Wire::new(&kourier, &gateway, 2, next());
    assert!(
        to_gateway.open_sent(&b.sent(), 1).is_some(),
        "no nickname, zeroes"
    );
    // 33 bytes whose 32nd byte is the middle of a character: cut BEFORE that character.
    let long = format!("{}{}{}", "a".repeat(30), "\u{e9}", "\u{e9}");
    assert_eq!(long.len(), 34);
    let mut cut = Wire::new(&kourier, &gateway, 3, next());
    cut.nickname = Some(long.clone());
    let mut field = [0u8; LEGACY_NICKNAME];
    field.copy_from_slice(&long.as_bytes()[..32]);
    cut.legacy = Some(field);
    assert!(
        to_gateway.open_sent(&cut.sent(), 2).is_some(),
        "cut at the character, 32 bytes"
    );
    let odd = format!("{}{}", "a".repeat(31), "\u{e9}");
    let mut odd_wire = Wire::new(&kourier, &gateway, 4, next());
    odd_wire.nickname = Some(odd.clone());
    let mut field = [0u8; LEGACY_NICKNAME];
    field[..31].copy_from_slice(&odd.as_bytes()[..31]);
    odd_wire.legacy = Some(field);
    assert!(
        to_gateway.open_sent(&odd_wire.sent(), 3).is_some(),
        "cut at the character, 31 bytes"
    );

    // A Gateway ID with the nickname missing from the head.
    let mut missing = Wire::new(&kourier, &gateway, 5, next());
    missing.nickname = Some("Kourier".to_owned());
    missing.legacy = Some([0; LEGACY_NICKNAME]);
    assert!(to_gateway.open_sent(&missing.sent(), 4).is_none());
    expected.push("peer-legacy-nickname-differs");
    // A byte cut in the middle of a character.
    let mut torn = Wire::new(&kourier, &gateway, 6, next());
    torn.nickname = Some(long.clone());
    let mut field = [0u8; LEGACY_NICKNAME];
    field[..31].copy_from_slice(&long.as_bytes()[..31]);
    torn.legacy = Some(field);
    assert!(to_gateway.open_sent(&torn.sent(), 5).is_none());
    expected.push("peer-legacy-nickname-differs");
    // A nickname that is not the metadata's.
    let mut other = Wire::new(&kourier, &gateway, 7, next());
    other.nickname = Some("Kourier".to_owned());
    let mut field = [0u8; LEGACY_NICKNAME];
    field[..5].copy_from_slice(b"Other");
    other.legacy = Some(field);
    assert!(to_gateway.open_sent(&other.sent(), 6).is_none());
    expected.push("peer-legacy-nickname-differs");
    // A person gets zeroes, even when the metadata has a nickname.
    let mut c = Wire::new(&kourier, &person, 8, next());
    c.nickname = Some("Kourier".to_owned());
    assert!(
        to_person.open_sent(&c.sent(), 7).is_some(),
        "zeroes for a person"
    );
    let mut d = Wire::new(&kourier, &person, 9, next());
    d.nickname = Some("Kourier".to_owned());
    let mut field = [0u8; LEGACY_NICKNAME];
    field[..7].copy_from_slice(b"Kourier");
    d.legacy = Some(field);
    assert!(to_person.open_sent(&d.sent(), 8).is_none());
    expected.push("peer-legacy-nickname-not-zero");
    assert_eq!(w.violations().rules(), expected);
}

/// Each breach of the client has its own rule, and the message is not reported as opened.
#[test]
fn every_breach_of_the_client_has_its_own_rule() {
    let w = world(404);
    let kourier = w.add_identity(KOURIER);
    let person = w.add_identity(PEER);
    let mut peer = w.peer(PEER);
    let mut expected: Vec<&str> = Vec::new();

    // Breaches of the Wire's fields: the bytes are well formed, one rule is not met.
    #[allow(clippy::type_complexity)]
    let by_field: Vec<(&str, fn(&mut Wire<'_>), &str)> = vec![
        (
            "no metadata box",
            |m| m.meta = Meta::Absent,
            "peer-no-metadata-box",
        ),
        (
            "the metadata under the message key",
            |m| m.meta = Meta::WrongKey,
            "peer-metadata-does-not-open",
        ),
        (
            "the metadata is no protobuf",
            |m| m.meta = Meta::Garbage,
            "peer-metadata-undecodable",
        ),
        (
            "the metadata names another id",
            |m| m.meta_id = Some(m.id ^ 1),
            "peer-metadata-id",
        ),
        (
            "the head's seconds are not the metadata's",
            |m| m.head_s = Some(1),
            "peer-created-at-differs",
        ),
        (
            "the nickname is the ID",
            |m| m.nickname = Some(KOURIER.to_owned()),
            "peer-nickname-is-the-id",
        ),
        ("type 0xff", |m| m.ty = 0xff, "peer-type-0xff"),
        ("type 0x00", |m| m.ty = 0x00, "peer-type-unknown"),
        ("type 0xee", |m| m.ty = 0xee, "peer-type-unknown"),
        (
            "an unknown flag bit",
            |m| m.flags = 0x08,
            "peer-flags-unknown-bits",
        ),
        (
            "the last padding byte is zero",
            |m| m.padding = Some(vec![0]),
            "peer-padding",
        ),
        (
            "the padding is longer than the data",
            |m| m.padding = Some(vec![200]),
            "peer-padding",
        ),
        (
            "the padding bytes differ",
            |m| m.padding = Some(vec![5, 5, 5, 5, 4]),
            "peer-padding",
        ),
        (
            "no data and no padding",
            |m| {
                m.data = Vec::new();
                m.padding = Some(Vec::new());
            },
            "peer-padding",
        ),
        (
            "padded data under 32 bytes",
            |m| m.padding = Some(vec![5; 5]),
            "peer-padded-data-under-32",
        ),
    ];
    for (n, (name, bend, rule)) in by_field.iter().enumerate() {
        let n8 = u8::try_from(n).expect("few cases");
        let mut wire = Wire::new(&kourier, &person, 0x100 + u64::from(n8), n8 + 1);
        bend(&mut wire);
        let before = w.violations().rules().len();
        assert!(
            peer.open_sent(&wire.sent(), n).is_none(),
            "{name}: the peer opened it"
        );
        assert_eq!(
            w.violations().rules()[before..],
            [*rule],
            "{name}: the rule"
        );
        expected.push(*rule);
    }

    // Breaches of the bytes: the struct itself is damaged.
    #[allow(clippy::type_complexity)]
    let by_bytes: Vec<(&str, fn(&mut Sent), &str)> = vec![
        (
            "the message box is tampered with",
            |s| {
                let last = s.bytes.len() - 1;
                s.bytes[last] ^= 1;
            },
            "peer-box-does-not-open",
        ),
        (
            "the nonce is cut short",
            |s| {
                let metadata = usize::from(u16::from_le_bytes([s.bytes[30], s.bytes[31]]));
                s.bytes.truncate(HEAD + metadata + 23);
            },
            "peer-head-malformed",
        ),
        (
            "the message box is empty",
            |s| {
                let metadata = usize::from(u16::from_le_bytes([s.bytes[30], s.bytes[31]]));
                s.bytes.truncate(HEAD + metadata + 24);
            },
            "peer-box-does-not-open",
        ),
        (
            "the struct ends in the head",
            |s| s.bytes.truncate(HEAD - 1),
            "peer-head-malformed",
        ),
        (
            "the sender is nobody in the table",
            |s| s.bytes[..8].copy_from_slice(b"GHOST001"),
            "peer-sender-unknown",
        ),
    ];
    for (n, (name, bend, rule)) in by_bytes.iter().enumerate() {
        let n8 = u8::try_from(n).expect("few cases");
        let mut sent = Wire::new(&kourier, &person, 0x200 + u64::from(n8), 100 + n8).sent();
        bend(&mut sent);
        let before = w.violations().rules().len();
        assert!(
            peer.open_sent(&sent, 100 + n).is_none(),
            "{name}: the peer opened it"
        );
        assert_eq!(
            w.violations().rules()[before..],
            [*rule],
            "{name}: the rule"
        );
        expected.push(*rule);
    }
    // None of the broken messages is reported as opened.
    assert!(peer.opened().is_empty(), "something was opened");
    assert_eq!(w.violations().rules(), expected);
}

/// The receiver's order (steps 2, 13, 14): a used nonce is discarded first (the same bytes again),
/// then the id; another message under a used nonce is the client's breach. Ids are per sender.
#[test]
fn the_nonce_and_the_id_are_checked_in_the_specs_order() {
    let w = world(405);
    let kourier = w.add_identity(KOURIER);
    let second = w.add_identity("KOURIER2");
    let person = w.add_identity(PEER);
    let mut peer = w.peer(PEER);

    let first = Wire::new(&kourier, &person, 0x51, 1);
    let first_sent = first.sent();
    let opened = peer.open_sent(&first_sent, 0).expect("opens");
    assert_eq!(opened.discard, None, "the first copy");

    // The same bytes again (a resend): discarded by the nonce, and it is no breach.
    let again = peer.open_sent(&first_sent, 1).expect("opens");
    assert_eq!(again.discard, Some(Discard::Nonce), "the same bytes");
    assert_eq!(again.body, opened.body);
    assert_eq!(again.message_id, opened.message_id);

    // The same id under a fresh nonce: discarded by the id, and the box is opened in full.
    let mut renewed = Wire::new(&kourier, &person, 0x51, 2);
    renewed.data = b"other padding".to_vec();
    let renewed_sent = renewed.sent();
    let by_id = peer.open_sent(&renewed_sent, 2).expect("opens");
    assert_eq!(by_id.discard, Some(Discard::Id), "a fresh nonce, the id");
    assert_eq!(by_id.body, b"other padding".to_vec());

    // That one again: the nonce is checked before the id.
    let again = peer.open_sent(&renewed_sent, 3).expect("opens");
    assert_eq!(again.discard, Some(Discard::Nonce), "both known: the nonce");

    // A second sender may use the same id: ids are per sender.
    let other_sender = Wire::new(&second, &person, 0x51, 3);
    let got = peer.open_sent(&other_sender.sent(), 4).expect("opens");
    assert_eq!(got.discard, None, "the id of another sender");

    // A new id under a used nonce but other bytes: the client reused a nonce.
    let mut reused = Wire::new(&kourier, &person, 0x52, 1);
    reused.data = b"different".to_vec();
    assert!(peer.open_sent(&reused.sent(), 5).is_none());
    assert_eq!(peer.opened().len(), 5, "what was opened, discards included");
    assert_eq!(
        w.violations().rules(),
        vec!["peer-nonce-reused-with-other-bytes"]
    );
}

/// PKCS#7 as the container says (1 to 255 trailing bytes of the pad length) and the 32-byte floor
/// of the padded data, at the edge on both sides.
#[test]
fn padding_is_pkcs7_and_the_padded_data_has_32_bytes_at_least() {
    // `unpad` by literal bytes.
    assert_eq!(unpad(&[7; 7]), Some(&[] as &[u8]), "all padding");
    assert_eq!(unpad(&[1, 2, 3, 4, 3, 3]), None, "the bytes differ");
    assert_eq!(unpad(&[1, 2, 3, 3, 3, 3]), Some(&[1, 2, 3][..]), "3 of 3");
    assert_eq!(unpad(&[1, 2, 3, 3, 3]), Some(&[1, 2][..]), "3 of 3");
    assert_eq!(unpad(&[9, 1]), Some(&[9][..]), "one byte of padding");
    assert_eq!(unpad(&[1]), Some(&[] as &[u8]));
    assert_eq!(unpad(&[]), None, "nothing");
    assert_eq!(unpad(&[1, 0]), None, "a pad length of zero");
    assert_eq!(unpad(&[2]), None, "a pad longer than the data");
    let mut longest = vec![5; 100];
    longest.extend(vec![255; 255]);
    assert_eq!(unpad(&longest).map(<[u8]>::len), Some(100), "255 bytes");

    let w = world(406);
    let kourier = w.add_identity(KOURIER);
    let person = w.add_identity(PEER);
    let mut peer = w.peer(PEER);
    let mut n = 0u8;
    // data + padding of exactly 32 bytes: fine, whatever the split.
    for (data, pad) in [
        (0usize, 32usize),
        (1, 31),
        (30, 2),
        (31, 1),
        (4000, 255),
        (32, 1),
    ] {
        n += 1;
        let mut wire = Wire::new(&kourier, &person, u64::from(n), n);
        wire.data = vec![0x5a; data];
        wire.padding = Some(vec![u8::try_from(pad).expect("a pad below 256"); pad]);
        let got = peer
            .open_sent(&wire.sent(), usize::from(n))
            .unwrap_or_else(|| panic!("data {data}, pad {pad} was refused"));
        assert_eq!(got.body.len(), data, "data {data}, pad {pad}");
    }
    // 31 bytes in all: under the floor.
    n += 1;
    let mut short = Wire::new(&kourier, &person, u64::from(n), n);
    short.data = vec![0x5a; 30];
    short.padding = Some(vec![1]);
    assert!(peer.open_sent(&short.sent(), usize::from(n)).is_none());
    assert_eq!(
        w.violations().rules(),
        vec!["peer-padded-data-under-32"],
        "31 bytes"
    );
}
