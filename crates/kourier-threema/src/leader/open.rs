//! THE OPEN OF AN INCOMING MESSAGE (TH6 L3): the `message-with-metadata-box` a chat server delivers to the leader
//! (`TP/csp.struct.yml:627-660, 873-930`), read field by field, and its two boxes opened with the key the user shares
//! with the SENDER (`ClientKey::derive_csp_e2e_key`, then libthreema's patch-P5 `decrypt_metadata` and
//! `decrypt_message`). This file and the rest of `src/leader/` are the only places of `src/` besides `src/reflect/` that
//! may name the two opens (`tests/builder.rs` rule 14); no seal and no `hash_with` here.
//!
//! NO STRICTER THAN THE PHONE (critic T-L4): the head is read as libthreema reads it
//! (`LT/csp_e2e/message/payload.rs`, `IncomingMessageWithMetadataBox::try_from`), except that the LEGACY NICKNAME is
//! never a reason to discard: it is decoded only when the steps ask for it (22.2), and bytes that are not UTF-8 are
//! `None` there. The padding rule is libthreema's (`LT/csp_e2e/message/task/incoming.rs:257-283`): only the LAST byte
//! of the container says how long the padding is (greater than zero, at most the container's length); the bytes of
//! the padding are not compared with it. A receiver that is not the user is the steps' discard (step 3), not the
//! parse's.
//!
//! Every failure is a RULE NAME of the steps (`leader::steps`): a discard the spec makes and acks (steps 6-8), never a
//! hold. Pure: no I/O, no clock, no log; `Debug` prints lengths and flags, never an ID, a nickname, a nonce or a body.

use std::fmt;

use libthreema::common::keys::CspE2eKey;
use libthreema::common::{MessageId, MessageMetadata, Nonce, ThreemaId};
use libthreema::protobuf::csp_e2e;
use prost::Message as _;
use zeroize::Zeroizing;

/// The fixed head of the payload before the metadata box: sender (8), receiver (8), message id (8), created-at (4),
/// flags (1), reserved (1), metadata length (2), legacy sender nickname (32). The same number the send writes
/// (`send::encode::HEAD_LEN`).
pub(crate) const HEAD_LEN: usize = 64;

/// The bytes of the legacy sender nickname field.
const LEGACY_LEN: usize = 32;

/// The tag of an XSalsa20-Poly1305 box (each box is the tag, then the ciphertext).
const TAG_LEN: usize = 16;

/// The payload is shorter than its head says (libthreema: `PayloadDecodingFailed`, discarded and acked, no nonce).
pub(crate) const RULE_HEAD: &str = "csp-head";
/// The metadata box does not open or its `MessageMetadata` does not decode (step 6).
pub(crate) const RULE_METADATA: &str = "csp-metadata";
/// The message box does not open with the sender's key (step 7).
pub(crate) const RULE_UNDECRYPTABLE: &str = "csp-undecryptable";
/// The opened container has no valid padding or no type byte (step 7).
pub(crate) const RULE_CONTAINER: &str = "csp-container";
/// The outer type is `0xff` (step 8, the legacy vouch mechanism).
pub(crate) const RULE_LEGACY_FF: &str = "csp-legacy-ff";

/// The clear head of one incoming payload, borrowed from its bytes.
pub(crate) struct Boxed<'a> {
    /// The sender's 8 bytes (libthreema's CSP layer already read them as a `ThreemaId`).
    pub(crate) sender: [u8; ThreemaId::LENGTH],
    /// The receiver's 8 bytes, compared with the user's at step 3.
    pub(crate) receiver: [u8; ThreemaId::LENGTH],
    /// The message id (`u64-le`).
    pub(crate) message_id: MessageId,
    /// The legacy created-at, seconds (the metadata's is preferred, T-L3).
    pub(crate) created_at_s: u32,
    /// The CSP message flags.
    pub(crate) flags: u8,
    /// The zero-padded legacy sender nickname, as it came.
    pub(crate) legacy_nickname: [u8; LEGACY_LEN],
    /// The metadata box (tag, then ciphertext); `None` when its length is zero (a legacy sender).
    pub(crate) metadata: Option<&'a [u8]>,
    /// The nonce of both boxes (`message-and-metadata-nonce`).
    pub(crate) nonce: Nonce,
    /// The message box (tag, then ciphertext).
    pub(crate) message: &'a [u8],
}

impl fmt::Debug for Boxed<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Boxed")
            .field("flags", &self.flags)
            .field("metadata", &self.metadata.map(<[u8]>::len))
            .field("message", &self.message.len())
            .finish_non_exhaustive()
    }
}

impl Boxed<'_> {
    /// The legacy sender nickname as step 22.2 reads it: the bytes up to the first zero, trimmed; `Some("")` for all
    /// zeroes (the field always holds a value, footnote 1); `None` when the bytes are not UTF-8 (never a discard,
    /// T-L4).
    pub(crate) fn legacy_nickname(&self) -> Option<String> {
        let end = self
            .legacy_nickname
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(LEGACY_LEN);
        let bytes = self.legacy_nickname.get(..end)?;
        std::str::from_utf8(bytes).ok().map(|n| n.trim().to_owned())
    }
}

/// Reads the head of `bytes` (the whole `message-with-metadata-box` payload). `Err(RULE_HEAD)` when it is shorter than
/// the head, the metadata box it announces, the nonce and one tag. A metadata box shorter than a tag is the open's
/// failure ([`RULE_METADATA`]), as a box that does not open.
pub(crate) fn parse(bytes: &[u8]) -> Result<Boxed<'_>, &'static str> {
    let head = bytes.get(..HEAD_LEN).ok_or(RULE_HEAD)?;
    let field = |at: usize, len: usize| head.get(at..at + len).ok_or(RULE_HEAD);
    let fixed8 = |at: usize| -> Result<[u8; 8], &'static str> {
        field(at, 8)?.try_into().map_err(|_| RULE_HEAD)
    };
    let sender = fixed8(0)?;
    let receiver = fixed8(8)?;
    let message_id = MessageId(u64::from_le_bytes(fixed8(16)?));
    let created_at_s = u32::from_le_bytes(field(24, 4)?.try_into().map_err(|_| RULE_HEAD)?);
    let flags = *head.get(28).ok_or(RULE_HEAD)?;
    let metadata_len = usize::from(u16::from_le_bytes(
        field(30, 2)?.try_into().map_err(|_| RULE_HEAD)?,
    ));
    let legacy_nickname: [u8; LEGACY_LEN] =
        field(32, LEGACY_LEN)?.try_into().map_err(|_| RULE_HEAD)?;
    let rest = bytes.get(HEAD_LEN..).ok_or(RULE_HEAD)?;
    let (metadata, rest) = rest.split_at_checked(metadata_len).ok_or(RULE_HEAD)?;
    let (nonce, message) = rest.split_at_checked(Nonce::LENGTH).ok_or(RULE_HEAD)?;
    if message.len() < TAG_LEN {
        return Err(RULE_HEAD);
    }
    Ok(Boxed {
        sender,
        receiver,
        message_id,
        created_at_s,
        flags,
        legacy_nickname,
        metadata: (metadata_len > 0).then_some(metadata),
        nonce: Nonce(nonce.try_into().map_err(|_| RULE_HEAD)?),
        message,
    })
}

/// What the two boxes hold.
pub(crate) struct Opened {
    /// The decoded `csp-e2e.MessageMetadata` (its nickname trimmed, libthreema's conversion); `None` without a
    /// metadata box.
    pub(crate) metadata: Option<MessageMetadata>,
    /// The container's outer type byte (known or not: the steps decide; `0xff` never comes here).
    pub(crate) outer_type: u8,
    /// The container's data after the type, UNPADDED: what a reflected `IncomingMessage.body` carries
    /// (`TP/md-d2d.proto:284-285`).
    pub(crate) data: Zeroizing<Vec<u8>>,
}

impl fmt::Debug for Opened {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Opened")
            .field("metadata", &self.metadata.is_some())
            .field("outer_type", &self.outer_type)
            .field("data", &self.data.len())
            .finish()
    }
}

/// Opens both boxes of `boxed` with `key`, the key the user shares with the sender (steps 6-8). Each failure is the
/// rule of its step.
pub(crate) fn open(key: &CspE2eKey, boxed: &Boxed<'_>) -> Result<Opened, &'static str> {
    let metadata = match boxed.metadata {
        None => None,
        Some(sealed) => {
            let plain = Zeroizing::new(
                key.decrypt_metadata(&boxed.nonce, sealed)
                    .map_err(|_| RULE_METADATA)?,
            );
            let decoded =
                csp_e2e::MessageMetadata::decode(plain.as_slice()).map_err(|_| RULE_METADATA)?;
            Some(MessageMetadata::from(decoded))
        }
    };
    let container = Zeroizing::new(
        key.decrypt_message(&boxed.nonce, boxed.message)
            .map_err(|_| RULE_UNDECRYPTABLE)?,
    );
    // libthreema's padding rule: the last byte, greater than zero, at most the length.
    let pad = usize::from(*container.last().ok_or(RULE_CONTAINER)?);
    if pad == 0 {
        return Err(RULE_CONTAINER);
    }
    let unpadded = container
        .len()
        .checked_sub(pad)
        .and_then(|end| container.get(..end))
        .ok_or(RULE_CONTAINER)?;
    let (&outer_type, data) = unpadded.split_first().ok_or(RULE_CONTAINER)?;
    if outer_type == 0xff {
        return Err(RULE_LEGACY_FF);
    }
    Ok(Opened {
        metadata,
        outer_type,
        data: Zeroizing::new(data.to_vec()),
    })
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU8;

    use libthreema::common::keys::ClientKey;
    use libthreema::protobuf::common::CspE2eMessageType;

    use super::*;
    use crate::send::encode::{self, Head, padded_container, seal_box};

    const PEER: &str = "A6PER002";
    const OWN: &str = "A6OWN001";

    fn id(text: &str) -> ThreemaId {
        ThreemaId::try_from(text).expect("a synthetic Threema ID")
    }

    /// The two keys of one exchange: the peer's (sealing) and the user's (opening).
    fn keys() -> (CspE2eKey, CspE2eKey) {
        let peer = ClientKey::from([0x11; 32]);
        let own = ClientKey::from([0x22; 32]);
        (
            peer.derive_csp_e2e_key(&own.public_key())
                .expect("contributory"),
            own.derive_csp_e2e_key(&peer.public_key())
                .expect("contributory"),
        )
    }

    /// A payload the SEND side writes (its encode is the other implementation here: `send::encode`), with or without
    /// the metadata box.
    fn payload(data: &[u8], nickname: Option<&str>, meta: bool, pad: u8) -> Vec<u8> {
        let (seal, _) = keys();
        let nonce = Nonce([7; 24]);
        let container = padded_container(
            CspE2eMessageType::Text,
            data,
            NonZeroU8::new(pad).expect("a pad"),
        );
        let metadata = encode::metadata(MessageId(0x0102), 1_700_000_000_123, nickname);
        let sealed = seal_box(&seal, &nonce, metadata, container).expect("sealed");
        let head = Head {
            sender: id(PEER),
            receiver: id(OWN),
            message_id: MessageId(0x0102),
            created_at_ms: 1_700_000_000_123,
            flags: 0x01,
            nickname,
        };
        let meta_box: &[u8] = if meta { &sealed.metadata } else { &[] };
        encode::layout(&head, meta_box, &nonce, &sealed.message).expect("a layout")
    }

    /// Every field of the head at its offset, both boxes open, the metadata decodes (nickname trimmed), the data is
    /// unpadded; without metadata the box is `None` and the head still reads.
    #[test]
    fn a_sealed_payload_parses_and_opens_to_its_fields() {
        let (_, key) = keys();
        let bytes = payload(b"hello", Some(" Pat "), true, 40);
        let boxed = parse(&bytes).expect("parses");
        assert_eq!(&boxed.sender, PEER.as_bytes());
        assert_eq!(&boxed.receiver, OWN.as_bytes());
        assert_eq!(boxed.message_id, MessageId(0x0102));
        assert_eq!(boxed.created_at_s, 1_700_000_000);
        assert_eq!(boxed.flags, 0x01);
        assert_eq!(boxed.nonce.0, [7; 24]);
        assert_eq!(
            boxed.legacy_nickname(),
            Some(String::new()),
            "zeroes: the empty name"
        );
        let opened = open(&key, &boxed).expect("opens");
        let meta = opened.metadata.expect("metadata");
        assert_eq!(meta.message_id, MessageId(0x0102));
        assert_eq!(meta.created_at, 1_700_000_000_123);
        assert_eq!(meta.nickname.as_deref(), Some("Pat"), "trimmed");
        assert_eq!(opened.outer_type, 0x01);
        assert_eq!(opened.data.as_slice(), b"hello");

        let legacy = payload(b"hello", None, false, 1);
        let boxed = parse(&legacy).expect("parses");
        assert!(boxed.metadata.is_none());
        let opened = open(&key, &boxed).expect("opens");
        assert!(opened.metadata.is_none());
        assert_eq!(opened.data.as_slice(), b"hello");
        assert!(
            !format!("{boxed:?}{opened:?}").contains(PEER),
            "no ID in Debug"
        );
    }

    /// THE PADDING RULE IS LIBTHREEMA'S (T-L4): the last byte alone counts; non-uniform padding bytes open; a zero or
    /// an over-long pad, an empty container and the type `0xff` are the step's discards.
    #[test]
    fn only_the_last_padding_byte_counts_and_the_failures_are_their_steps_rules() {
        let (seal, key) = keys();
        let nonce = Nonce([9; 24]);
        let sealed_with = |container: Vec<u8>| -> Vec<u8> {
            let sealed = seal
                .encrypt_message(&nonce, container)
                .expect("the cipher seals");
            let head = Head {
                sender: id(PEER),
                receiver: id(OWN),
                message_id: MessageId(1),
                created_at_ms: 0,
                flags: 0,
                nickname: None,
            };
            encode::layout(&head, &[], &nonce, &sealed).expect("a layout")
        };
        let opened = |container: Vec<u8>| {
            let bytes = sealed_with(container);
            let boxed = parse(&bytes).expect("parses");
            open(&key, &boxed).map(|o| (o.outer_type, o.data.to_vec()))
        };
        // Non-uniform padding: 0xaa 0xbb 0x03 is a pad of 3 by its last byte.
        assert_eq!(
            opened(vec![0x01, b'h', b'i', 0xaa, 0xbb, 0x03]),
            Ok((0x01, b"hi".to_vec()))
        );
        assert_eq!(
            opened(vec![0x01, b'h', 0x00]),
            Err(RULE_CONTAINER),
            "a zero pad"
        );
        assert_eq!(
            opened(vec![0x01, 0x09]),
            Err(RULE_CONTAINER),
            "a pad past the start"
        );
        assert_eq!(
            opened(vec![0x01, 0x02]),
            Err(RULE_CONTAINER),
            "no type byte left"
        );
        assert_eq!(
            opened(vec![0x01]),
            Err(RULE_CONTAINER),
            "a pad of 1 eats the type"
        );
        assert_eq!(opened(vec![0xff, b'x', 0x01]), Err(RULE_LEGACY_FF));
        assert_eq!(
            opened(vec![0x7e, 0x01]),
            Ok((0x7e, vec![])),
            "an unknown type is the steps'"
        );
        // A box another key sealed does not open.
        let mut bytes = sealed_with(vec![0x01, b'h', 0x01]);
        let last = bytes.len() - 1;
        bytes[last] ^= 0x01;
        let boxed = parse(&bytes).expect("parses");
        assert_eq!(open(&key, &boxed).map(|_| ()), Err(RULE_UNDECRYPTABLE));
        // A metadata box that does not open.
        let mut bytes = payload(b"x", Some("n"), true, 5);
        bytes[HEAD_LEN] ^= 0x01;
        let boxed = parse(&bytes).expect("parses");
        assert_eq!(open(&key, &boxed).map(|_| ()), Err(RULE_METADATA));
    }

    /// The head's own limits: too short for the head, the announced metadata box, the nonce or one tag.
    #[test]
    fn a_short_payload_is_the_head_rule() {
        let bytes = payload(b"hello", Some("Pat"), true, 3);
        assert!(parse(&bytes).is_ok());
        for cut in [0, 10, HEAD_LEN - 1, HEAD_LEN, HEAD_LEN + 20] {
            assert_eq!(
                parse(&bytes[..cut]).map(|_| ()),
                Err(RULE_HEAD),
                "cut at {cut}"
            );
        }
        let meta_len = usize::from(u16::from_le_bytes([bytes[30], bytes[31]]));
        let tail = HEAD_LEN + meta_len + Nonce::LENGTH;
        assert_eq!(
            parse(&bytes[..tail + TAG_LEN - 1]).map(|_| ()),
            Err(RULE_HEAD)
        );
        assert!(
            parse(&bytes[..tail + TAG_LEN]).is_ok(),
            "a tag and no ciphertext reads"
        );
    }

    /// The legacy nickname (T-L4): the bytes up to the first zero, trimmed; bytes that are not UTF-8 are `None`, and
    /// the payload still parses.
    #[test]
    fn the_legacy_nickname_is_read_only_when_asked_and_never_refuses() {
        let mut bytes = payload(b"x", None, false, 1);
        bytes[32..32 + 6].copy_from_slice(b" Ann  ");
        assert_eq!(
            parse(&bytes).expect("parses").legacy_nickname().as_deref(),
            Some("Ann")
        );
        bytes[32..32 + 3].copy_from_slice(&[0xff, 0xfe, 0x00]);
        let boxed = parse(&bytes).expect("a bad legacy nickname still parses");
        assert_eq!(boxed.legacy_nickname(), None);
        bytes[32..64].copy_from_slice(&[b'a'; 32]);
        assert_eq!(
            parse(&bytes)
                .expect("parses")
                .legacy_nickname()
                .map(|n| n.len()),
            Some(32),
            "all 32 bytes when no zero ends it"
        );
    }
}
