//! THE ENCODE (TH6a S2): the bytes a send puts on the wire and into the device group, written to the pinned spec and
//! sealed with libthreema's patch-P5 boxes. Pure: bytes in, bytes out, the randomness (the pad length, a nonce)
//! handed in by the caller so a test can fix it. THIS FILE ALONE names the three seals of a sent message
//! (`seal_reflected`, `encrypt_message`, `encrypt_metadata`; source check, rule 14); every open and decrypt stays in
//! `src/reflect/`.
//!
//! The pieces, in the order a frame is built (`csp.struct.yml`; no P8, nothing asked of upstream):
//!
//! - [`padded_container`]: `type || data || PKCS#7`. A pad of 1 to 255 bytes, raised so that `data` plus the pad is at
//!   least 32 bytes (the spec's step 3 of `container`).
//! - [`metadata`]: the `csp-e2e.MessageMetadata` bytes (message id, created-at in ms, nickname), padded by
//!   libthreema's own conversion.
//! - [`seal_box`]: the metadata under the Message Metadata Key and the container under the Message Key, one nonce for
//!   both (`message-and-metadata-nonce`).
//! - [`layout`]: the `message-with-metadata-box` payload, field by field. No crypto here, so its bytes can be pinned
//!   offset by offset.
//! - [`outgoing_message`], [`sent_update`], [`envelope_bytes`], [`seal_envelope`]: the `d2d.Envelope` of a reflected
//!   `OutgoingMessage` or of its `Sent` update, padded to libthreema's numbers (`PaddedMessage` is crate-private
//!   there, so the padding field is filled here), sealed with the Device Group Reflect Key.
//!
//! NEVER ENCODE AGAIN UNDER A STORED NONCE. Every pad above is random, so a second encode of the same message under
//! the same nonce would reuse an XSalsa20 keystream over different plaintext. A row is sealed once, in its prepare
//! zone, and its sealed bytes are stored; a resend writes them as they are.
//!
//! Nothing here logs a body, an ID, a key or a nonce, and `Debug` of the input types prints lengths only. Plaintext
//! is held in `Zeroizing`.

use std::fmt;
use std::num::NonZeroU8;

use libthreema::common::keys::{CspE2eKey, DeviceGroupKey};
use libthreema::common::{MessageId, MessageMetadata, Nonce, ThreemaId};
use libthreema::protobuf::common::CspE2eMessageType;
use libthreema::protobuf::d2d;
use prost::Message as _;
use rand::Rng as _;
use zeroize::Zeroizing;

/// The least bytes of `padded-data`, that is of `data` plus its padding (`container`, step 3).
const PAD_FLOOR: usize = 32;

/// The fixed part of a `message-with-metadata-box` payload before the metadata box: sender (8), receiver (8),
/// message id (8), created-at (4), flags (1), reserved (1), metadata length (2) and the legacy nickname (32).
pub(crate) const HEAD_LEN: usize = 64;

/// The bytes of the legacy sender nickname field.
const LEGACY_NICKNAME_LEN: usize = 32;

/// The least bytes of an envelope: what the message encodes to plus its padding (libthreema's
/// `minimum_total_length` for `d2d.Envelope`).
const ENVELOPE_PAD_FLOOR: usize = 64;

/// The padding of an envelope is drawn from `0..ENVELOPE_PAD_DRAWS` (libthreema's `maximum_padding_length`, its
/// draw is exclusive of the number).
pub(crate) const ENVELOPE_PAD_DRAWS: u16 = 512;

/// The fill of the envelope's padding (`33emafill`, libthreema's `PADDING_FILL_VALUE`).
const ENVELOPE_PAD_FILL: u8 = 0x33;

/// Why a frame or an envelope was not made. Carries nothing of the input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum EncodeError {
    /// The cipher refused the buffer.
    #[error("the cipher refused to seal")]
    Seal,
    /// The sealed metadata does not fit the payload's 16-bit length.
    #[error("the metadata box does not fit its length field")]
    MetadataTooLong,
}

/// The pad of a container: the drawn `draw` (1 to 255), raised when `data_len + draw` falls short of
/// [`PAD_FLOOR`] so that the sum is exactly the floor.
pub(crate) fn pad_length(data_len: usize, draw: NonZeroU8) -> u8 {
    if data_len.saturating_add(usize::from(draw.get())) < PAD_FLOOR {
        // Here `data_len` is under 31, so the pad is 2 to 32.
        u8::try_from(PAD_FLOOR - data_len).unwrap_or(draw.get())
    } else {
        draw.get()
    }
}

/// `type || data || PKCS#7` (`container`): the type byte, the data, and `pad` bytes each holding the value `pad`.
pub(crate) fn padded_container(
    ty: CspE2eMessageType,
    data: &[u8],
    draw: NonZeroU8,
) -> Zeroizing<Vec<u8>> {
    let pad = pad_length(data.len(), draw);
    let mut out = Zeroizing::new(Vec::with_capacity(1 + data.len() + usize::from(pad)));
    out.push(ty as u8);
    out.extend_from_slice(data);
    out.resize(1 + data.len() + usize::from(pad), pad);
    out
}

/// A random pad draw, 1 to 255 inclusive.
pub(crate) fn draw_pad() -> NonZeroU8 {
    NonZeroU8::new(rand::rng().random_range(1..=u8::MAX)).unwrap_or(NonZeroU8::MIN)
}

/// The `csp-e2e.MessageMetadata` of a message: its id (the same as the payload's), the time it was created (ms) and
/// the nickname the message distributes (`None`: the receiver keeps what it has; `Some("")`: the user cleared it).
/// Padded to at least 32 bytes by libthreema's conversion.
pub(crate) fn metadata(
    message_id: MessageId,
    created_at_ms: u64,
    nickname: Option<&str>,
) -> Zeroizing<Vec<u8>> {
    Zeroizing::new(
        MessageMetadata {
            message_id,
            created_at: created_at_ms,
            nickname: nickname.map(str::to_owned),
        }
        .into(),
    )
}

/// The two boxes of one receiver's frame.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Sealed {
    /// The metadata under the Message Metadata Key: tag, then ciphertext.
    pub(crate) metadata: Vec<u8>,
    /// The container under the Message Key: tag, then ciphertext.
    pub(crate) message: Vec<u8>,
}

impl fmt::Debug for Sealed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sealed")
            .field("metadata", &self.metadata.len())
            .field("message", &self.message.len())
            .finish()
    }
}

/// Seals `metadata` (MMK) and `container` (MK) under the one `nonce` of the frame, with the key shared with the
/// receiver (`ClientKey::derive_csp_e2e_key`; its `None` is a receiver with no usable key and never gets here).
///
/// # Errors
///
/// [`EncodeError::Seal`] when the cipher refuses a buffer.
pub(crate) fn seal_box(
    key: &CspE2eKey,
    nonce: &Nonce,
    metadata: Zeroizing<Vec<u8>>,
    container: Zeroizing<Vec<u8>>,
) -> Result<Sealed, EncodeError> {
    let mut metadata = metadata;
    let mut container = container;
    let metadata = key
        .encrypt_metadata(nonce, std::mem::take(&mut *metadata))
        .map_err(|_| EncodeError::Seal)?;
    let message = key
        .encrypt_message(nonce, std::mem::take(&mut *container))
        .map_err(|_| EncodeError::Seal)?;
    Ok(Sealed { metadata, message })
}

/// What the head of one receiver's payload says besides the boxes.
#[derive(Clone, Copy)]
pub(crate) struct Head<'a> {
    /// The own Threema ID.
    pub(crate) sender: ThreemaId,
    /// The receiver.
    pub(crate) receiver: ThreemaId,
    /// The message id, the same for every receiver of a group message.
    pub(crate) message_id: MessageId,
    /// When the message was created (ms), the same for every receiver of a group message.
    pub(crate) created_at_ms: u64,
    /// The CSP message flags of the type ([`super::props::Props::flags`]).
    pub(crate) flags: u8,
    /// The nickname the message distributes, the same that [`metadata`] holds; `None` for a type that does not
    /// distribute the profile. Only a Threema Gateway receiver ALSO gets it in the legacy field.
    pub(crate) nickname: Option<&'a str>,
}

impl fmt::Debug for Head<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Head { .. }")
    }
}

/// The created-at of the payload: seconds, saturating at what 32 bits hold.
fn created_at_seconds(created_at_ms: u64) -> u32 {
    u32::try_from(created_at_ms / 1000).unwrap_or(u32::MAX)
}

/// The legacy sender nickname field: the nickname for a Threema Gateway receiver (an ID starting with `*`) that the
/// message distributes it to, zeroes for everybody else; at most 32 bytes, cut at a character.
fn legacy_nickname(head: &Head<'_>) -> [u8; LEGACY_NICKNAME_LEN] {
    let mut field = [0_u8; LEGACY_NICKNAME_LEN];
    if head.receiver.is_gateway_id()
        && let Some(nickname) = head.nickname
    {
        let mut end = nickname.len().min(LEGACY_NICKNAME_LEN);
        while !nickname.is_char_boundary(end) {
            end -= 1;
        }
        if let (Some(to), Some(from)) = (field.get_mut(..end), nickname.as_bytes().get(..end)) {
            to.copy_from_slice(from);
        }
    }
    field
}

/// The `message-with-metadata-box` payload of one receiver, field by field (`csp.struct.yml`): sender, receiver,
/// message id (`u64-le`), created-at (`u32-le`, seconds), flags, reserved zero, the metadata box's length
/// (`u16-le`), the legacy nickname (32 bytes), the metadata box, the nonce, the message box.
///
/// # Errors
///
/// [`EncodeError::MetadataTooLong`] when the metadata box is over 65,535 bytes.
pub(crate) fn layout(
    head: &Head<'_>,
    metadata_box: &[u8],
    nonce: &Nonce,
    message_box: &[u8],
) -> Result<Vec<u8>, EncodeError> {
    let metadata_length =
        u16::try_from(metadata_box.len()).map_err(|_| EncodeError::MetadataTooLong)?;
    let mut out =
        Vec::with_capacity(HEAD_LEN + metadata_box.len() + Nonce::LENGTH + message_box.len());
    out.extend_from_slice(&head.sender.to_bytes());
    out.extend_from_slice(&head.receiver.to_bytes());
    out.extend_from_slice(&head.message_id.to_bytes());
    out.extend_from_slice(&created_at_seconds(head.created_at_ms).to_le_bytes());
    out.push(head.flags);
    out.push(0); // reserved
    out.extend_from_slice(&metadata_length.to_le_bytes());
    out.extend_from_slice(&legacy_nickname(head));
    out.extend_from_slice(metadata_box);
    out.extend_from_slice(&nonce.0);
    out.extend_from_slice(message_box);
    Ok(out)
}

/// An outgoing message as the device group is told of it.
#[derive(Clone, Copy)]
pub(crate) struct Outgoing<'a> {
    /// The conversation the message was sent in.
    pub(crate) conversation: &'a d2d::ConversationId,
    /// The message id.
    pub(crate) message_id: MessageId,
    /// When the message was created (ms).
    pub(crate) created_at_ms: u64,
    /// The CSP type.
    pub(crate) ty: CspE2eMessageType,
    /// The body: the unpadded `padded-data` ([`super::body`]).
    pub(crate) body: &'a [u8],
    /// The nonce of every box that went out for it (one per receiver that gets a box).
    pub(crate) nonces: &'a [Nonce],
}

impl fmt::Debug for Outgoing<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Outgoing")
            .field("body", &self.body.len())
            .field("nonces", &self.nonces.len())
            .finish()
    }
}

/// The reflected `OutgoingMessage` (`md-d2d.proto`).
pub(crate) fn outgoing_message(outgoing: &Outgoing<'_>) -> d2d::envelope::Content {
    d2d::envelope::Content::OutgoingMessage(d2d::OutgoingMessage {
        conversation: Some(outgoing.conversation.clone()),
        message_id: outgoing.message_id.0,
        thread_message_id: None,
        created_at: outgoing.created_at_ms,
        r#type: outgoing.ty as i32,
        body: outgoing.body.to_vec(),
        nonces: outgoing
            .nonces
            .iter()
            .map(|nonce| nonce.0.to_vec())
            .collect(),
    })
}

/// The `OutgoingMessageUpdate` that marks one message sent (acknowledged by the chat server).
pub(crate) fn sent_update(
    conversation: &d2d::ConversationId,
    message_id: MessageId,
) -> d2d::envelope::Content {
    use d2d::outgoing_message_update::{Sent, Update, update};

    d2d::envelope::Content::OutgoingMessageUpdate(d2d::OutgoingMessageUpdate {
        updates: vec![Update {
            conversation: Some(conversation.clone()),
            message_id: message_id.0,
            update: Some(update::Update::Sent(Sent {})),
        }],
    })
}

/// The padding of an envelope whose message encodes to `unpadded_len` bytes: the `draw` (0 to
/// [`ENVELOPE_PAD_DRAWS`] exclusive), raised so that message and padding together are at least
/// [`ENVELOPE_PAD_FLOOR`] bytes.
pub(crate) fn envelope_pad_length(unpadded_len: usize, draw: u16) -> usize {
    if unpadded_len.saturating_add(usize::from(draw)) < ENVELOPE_PAD_FLOOR {
        ENVELOPE_PAD_FLOOR - unpadded_len
    } else {
        usize::from(draw)
    }
}

/// A random envelope pad draw, `0..`[`ENVELOPE_PAD_DRAWS`].
pub(crate) fn draw_envelope_pad() -> u16 {
    rand::rng().random_range(0..ENVELOPE_PAD_DRAWS)
}

/// The bytes of a `d2d.Envelope` for `content`: this device, protocol version 0.3, the content, and the padding
/// [`envelope_pad_length`] gives for `draw`.
#[expect(
    deprecated,
    reason = "the spec's padding field: libthreema's padding trait is crate-private, so it is filled here to its numbers"
)]
pub(crate) fn envelope_bytes(
    device_id: u64,
    content: d2d::envelope::Content,
    draw: u16,
) -> Zeroizing<Vec<u8>> {
    let mut envelope = d2d::Envelope {
        padding: Vec::new(),
        device_id,
        protocol_version: d2d::ProtocolVersion::V03 as u32,
        content: Some(content),
    };
    let pad = envelope_pad_length(envelope.encoded_len(), draw);
    envelope.padding = vec![ENVELOPE_PAD_FILL; pad];
    Zeroizing::new(envelope.encode_to_vec())
}

/// Seals envelope bytes with the Device Group Reflect Key and a random nonce ahead. Returns the nonce (the D2D nonce
/// the store keeps, hashed) and `nonce || tag || ciphertext`, what `Reflect.envelope` carries.
///
/// # Errors
///
/// [`EncodeError::Seal`] when the cipher refuses the buffer.
pub(crate) fn seal_envelope(
    key: &DeviceGroupKey,
    envelope: Zeroizing<Vec<u8>>,
) -> Result<(Nonce, Vec<u8>), EncodeError> {
    let mut envelope = envelope;
    key.seal_reflected(std::mem::take(&mut *envelope))
        .map_err(|_| EncodeError::Seal)
}

#[cfg(test)]
mod tests {
    use libthreema::common::keys::ClientKey;
    use libthreema::protobuf::common::GroupIdentity;
    use libthreema::protobuf::csp_e2e;

    use super::*;

    const SENDER: &str = "A6SND001";
    const PEER: &str = "A6PER002";
    const GATEWAY: &str = "*A6GATE3";

    fn id(text: &str) -> ThreemaId {
        ThreemaId::try_from(text).expect("a synthetic Threema ID")
    }

    fn draw(n: u8) -> NonZeroU8 {
        NonZeroU8::new(n).expect("a draw from 1")
    }

    fn nonce(byte: u8) -> Nonce {
        Nonce([byte; Nonce::LENGTH])
    }

    /// A message id whose wire bytes (`to_bytes`) are 01..08.
    fn message_id() -> MessageId {
        MessageId::from([1, 2, 3, 4, 5, 6, 7, 8])
    }

    fn head<'a>(receiver: &str, nickname: Option<&'a str>) -> Head<'a> {
        Head {
            sender: id(SENDER),
            receiver: id(receiver),
            message_id: message_id(),
            created_at_ms: 1_700_000_000_999,
            flags: 0x01,
            nickname,
        }
    }

    // ---- the container's padding -------------------------------------------------------------------------------

    /// The pad rule of `container` as a table of literals: the drawn pad stands unless `data + pad` falls under 32,
    /// and then the pad is whatever makes the sum exactly 32.
    #[test]
    fn the_pad_rule_table() {
        // (data length, draw, expected pad)
        for (data, drawn, pad) in [
            (0, 1, 32),
            (0, 31, 32),
            (0, 32, 32),
            (0, 33, 33),
            (0, 255, 255),
            (1, 1, 31),
            (30, 1, 2),
            (30, 2, 2),
            (30, 3, 3),
            (31, 1, 1),
            (31, 2, 2),
            (32, 1, 1),
            (32, 255, 255),
            (33, 1, 1),
            (7000, 1, 1),
            (7000, 255, 255),
        ] {
            assert_eq!(
                pad_length(data, draw(drawn)),
                pad,
                "data {data} bytes, draw {drawn}"
            );
        }
    }

    /// The same rule as the spec states it, over every data length around the floor and every draw: a pad is 1 to 255
    /// bytes, `data + pad` is never under 32, and it is exactly 32 only where the draw alone fell short.
    #[test]
    fn the_pad_rule_holds_for_every_length_and_draw() {
        for data in 0..=70_usize {
            for drawn in 1..=u8::MAX {
                let pad = usize::from(pad_length(data, draw(drawn)));
                assert!(pad >= 1, "data {data}, draw {drawn}");
                assert!(data + pad >= 32, "data {data}, draw {drawn}: {pad}");
                if data + usize::from(drawn) >= 32 {
                    assert_eq!(pad, usize::from(drawn), "data {data}, draw {drawn}");
                } else {
                    assert_eq!(data + pad, 32, "data {data}, draw {drawn}");
                }
            }
        }
    }

    /// The container byte for byte (injected draws): the type, the data, then `pad` bytes holding `pad`; bodies of 0,
    /// 30, 31, 32 and 7000 bytes.
    #[test]
    fn a_padded_container_is_type_data_and_pkcs7() {
        // A body of 0 bytes: the type alone and 32 pad bytes (0x20).
        let empty = padded_container(CspE2eMessageType::Text, &[], draw(1));
        let mut expected = vec![0x01];
        expected.extend([0x20; 32]);
        assert_eq!(empty.as_slice(), expected.as_slice());

        // "hi": 2 bytes, draw 1, raised to 30 (0x1e).
        let hi = padded_container(CspE2eMessageType::Text, b"hi", draw(1));
        let mut expected = vec![0x01, b'h', b'i'];
        expected.extend([0x1e; 30]);
        assert_eq!(hi.as_slice(), expected.as_slice());

        // Around the floor: (data, draw) -> total bytes of `padded-data` (data + pad).
        for (data, drawn, padded_data) in [(30, 1, 32), (31, 1, 32), (32, 1, 33), (30, 9, 39)] {
            let body = vec![0xab; data];
            let out = padded_container(CspE2eMessageType::GroupText, &body, draw(drawn));
            assert_eq!(out[0], 0x41, "the type byte");
            assert_eq!(&out[1..=data], body.as_slice());
            assert_eq!(out.len() - 1, padded_data, "data {data}, draw {drawn}");
            let pad = *out.last().expect("a pad");
            assert_eq!(usize::from(pad), padded_data - data);
            assert!(out[1 + data..].iter().all(|b| *b == pad), "PKCS#7 bytes");
        }

        // A big body keeps the draw as its pad.
        let big = padded_container(CspE2eMessageType::File, &[0x5a; 7000], draw(255));
        assert_eq!(big.len(), 1 + 7000 + 255);
        assert!(big[7001..].iter().all(|b| *b == 0xff));
    }

    // ---- the metadata -----------------------------------------------------------------------------------------

    /// The metadata decodes to its literal fields, with the nickname absent, empty (cleared) or present, and its
    /// padding brings it to at least 32 bytes.
    #[test]
    #[expect(
        deprecated,
        reason = "the padding field is read to measure the padding"
    )]
    fn the_metadata_decodes_to_its_fields_and_is_padded() {
        for (nickname, expected) in [
            (None, None),
            (Some(""), Some("")),
            (Some("Kourier Test"), Some("Kourier Test")),
        ] {
            let bytes = metadata(message_id(), 1_700_000_000_123, nickname);
            let decoded = csp_e2e::MessageMetadata::decode(bytes.as_slice()).expect("decodes");
            assert_eq!(
                decoded.message_id,
                u64::from_le_bytes([1, 2, 3, 4, 5, 6, 7, 8])
            );
            assert_eq!(decoded.created_at, 1_700_000_000_123);
            assert_eq!(decoded.nickname.as_deref(), expected);
            let mut bare = decoded.clone();
            bare.padding.clear();
            assert!(
                bare.encoded_len() + decoded.padding.len() >= 32,
                "message {} + padding {}",
                bare.encoded_len(),
                decoded.padding.len()
            );
        }
    }

    // ---- the boxes ----------------------------------------------------------------------------------------------

    /// The metadata box opens only under the Message Metadata Key and the container only under the Message Key, both
    /// with the frame's one nonce and the key the receiver derives from the other side; a wrong nonce opens neither.
    #[test]
    fn the_boxes_open_with_the_receivers_keys_and_only_with_their_own() {
        let sender = ClientKey::random();
        let receiver = ClientKey::random();
        let sending = sender
            .derive_csp_e2e_key(&receiver.public_key())
            .expect("a contributory key");
        let receiving = receiver
            .derive_csp_e2e_key(&sender.public_key())
            .expect("a contributory key");
        let n = nonce(7);
        let meta = metadata(message_id(), 1_700_000_000_123, Some("n"));
        let meta_plain = meta.to_vec();
        let container = padded_container(CspE2eMessageType::Text, b"hello", draw(9));
        let container_plain = container.to_vec();

        let sealed = seal_box(&sending, &n, meta, container).expect("seals");
        assert_eq!(sealed.metadata.len(), meta_plain.len() + 16);
        assert_eq!(sealed.message.len(), container_plain.len() + 16);

        assert_eq!(
            receiving
                .decrypt_metadata(&n, &sealed.metadata)
                .expect("MMK opens it"),
            meta_plain
        );
        assert_eq!(
            receiving
                .decrypt_message(&n, &sealed.message)
                .expect("MK opens it"),
            container_plain
        );
        // The keys are not interchangeable and the nonce is part of the box.
        assert!(receiving.decrypt_message(&n, &sealed.metadata).is_err());
        assert!(receiving.decrypt_metadata(&n, &sealed.message).is_err());
        assert!(
            receiving
                .decrypt_message(&nonce(8), &sealed.message)
                .is_err()
        );
        let stranger = ClientKey::random()
            .derive_csp_e2e_key(&sender.public_key())
            .expect("a contributory key");
        assert!(stranger.decrypt_message(&n, &sealed.message).is_err());
    }

    // ---- the payload's layout ----------------------------------------------------------------------------------

    /// The payload offset by offset from the spec's fields, with fixed inputs and no crypto: the head is 64 bytes,
    /// then the metadata box, the 24-byte nonce, the message box.
    #[test]
    fn the_layout_puts_every_field_at_its_offset() {
        let metadata_box = vec![0xb1; 5];
        let message_box = vec![0xc2; 7];
        let out = layout(
            &head(PEER, Some("ignored")),
            &metadata_box,
            &nonce(0xee),
            &message_box,
        )
        .expect("lays out");
        assert_eq!(out.len(), 64 + 5 + 24 + 7);
        assert_eq!(&out[0..8], b"A6SND001", "sender");
        assert_eq!(&out[8..16], b"A6PER002", "receiver");
        assert_eq!(
            &out[16..24],
            &[1, 2, 3, 4, 5, 6, 7, 8],
            "message id, wire order"
        );
        // 1_700_000_000_999 ms is 1_700_000_000 s = 0x6553_F100, little-endian.
        assert_eq!(
            &out[24..28],
            &[0x00, 0xf1, 0x53, 0x65],
            "created-at, seconds"
        );
        assert_eq!(out[28], 0x01, "flags");
        assert_eq!(out[29], 0x00, "reserved");
        assert_eq!(&out[30..32], &[5, 0], "metadata length, little-endian");
        assert_eq!(
            &out[32..64],
            &[0_u8; 32],
            "no legacy nickname for a non-Gateway ID"
        );
        assert_eq!(&out[64..69], metadata_box.as_slice(), "metadata box");
        assert_eq!(&out[69..93], &[0xee; 24], "nonce");
        assert_eq!(&out[93..], message_box.as_slice(), "message box");
    }

    /// Created-at is in SECONDS: the millisecond clock divided by 1000, cut down, saturating at what 32 bits hold.
    #[test]
    fn the_layout_writes_created_at_in_seconds_saturating() {
        let at = |created_at_ms: u64| {
            let mut h = head(PEER, None);
            h.created_at_ms = created_at_ms;
            let out = layout(&h, &[], &nonce(1), &[]).expect("lays out");
            u32::from_le_bytes(out[24..28].try_into().expect("four bytes"))
        };
        assert_eq!(at(0), 0);
        assert_eq!(at(999), 0);
        assert_eq!(at(1_000), 1);
        assert_eq!(at(1_700_000_000_999), 1_700_000_000);
        assert_eq!(at(4_294_967_295_999), u32::MAX);
        assert_eq!(
            at(4_294_967_296_000),
            u32::MAX,
            "one second past 32 bits saturates"
        );
        assert_eq!(at(u64::MAX), u32::MAX);
    }

    /// The legacy nickname goes to a Threema Gateway ID alone, and only when the message distributes one; it is cut at
    /// 32 bytes on a character.
    #[test]
    fn the_legacy_nickname_goes_to_a_gateway_id_only() {
        let field = |receiver: &str, nickname: Option<&str>| {
            let out = layout(&head(receiver, nickname), &[], &nonce(1), &[]).expect("lays out");
            out[32..64].to_vec()
        };
        let mut gateway = b"Kourier Gateway".to_vec();
        gateway.resize(32, 0);
        assert_eq!(field(GATEWAY, Some("Kourier Gateway")), gateway);
        // Everybody else gets zeroes, the nickname or not.
        assert_eq!(field(PEER, Some("Kourier Gateway")), vec![0; 32]);
        assert_eq!(field("0123ABCD", Some("Kourier Gateway")), vec![0; 32]);
        // A type that distributes nothing, and a nickname cleared on purpose, leave the field empty for a Gateway too.
        assert_eq!(field(GATEWAY, None), vec![0; 32]);
        assert_eq!(field(GATEWAY, Some("")), vec![0; 32]);
        // 32 bytes exactly are kept; the 33rd is cut.
        let long = "x".repeat(40);
        assert_eq!(field(GATEWAY, Some(&long)), vec![b'x'; 32]);
        // Cut on a character: 31 ASCII bytes plus a two-byte letter would end mid-letter at 32.
        let split = format!("{}é", "y".repeat(31));
        let mut expected = vec![b'y'; 31];
        expected.push(0);
        assert_eq!(field(GATEWAY, Some(&split)), expected);
    }

    /// A metadata box over 65,535 bytes does not fit its length and is refused; 65,535 does.
    #[test]
    fn the_layout_refuses_a_metadata_box_over_its_length_field() {
        let h = head(PEER, None);
        let at_limit = layout(&h, &vec![0; 65_535], &nonce(1), &[]).expect("fits");
        assert_eq!(&at_limit[30..32], &[0xff, 0xff]);
        assert_eq!(
            layout(&h, &vec![0; 65_536], &nonce(1), &[]),
            Err(EncodeError::MetadataTooLong)
        );
    }

    // ---- the envelope -------------------------------------------------------------------------------------------

    const DEVICE: u64 = 0x0102_0304_0506_0708;

    fn decoded(bytes: &[u8]) -> d2d::Envelope {
        d2d::Envelope::decode(bytes).expect("decodes")
    }

    /// The reflected `OutgoingMessage` decodes to its literal fields, for a contact and for a group.
    #[test]
    fn the_outgoing_message_decodes_to_its_fields() {
        let contact = d2d::ConversationId {
            id: Some(d2d::conversation_id::Id::Contact(PEER.to_owned())),
        };
        let nonces = [nonce(1), nonce(2)];
        let content = outgoing_message(&Outgoing {
            conversation: &contact,
            message_id: message_id(),
            created_at_ms: 1_700_000_000_123,
            ty: CspE2eMessageType::Text,
            body: b"hello",
            nonces: &nonces,
        });
        let envelope = decoded(&envelope_bytes(DEVICE, content, 7));
        assert_eq!(envelope.device_id, DEVICE);
        assert_eq!(envelope.protocol_version, 3, "V0.3");
        let Some(d2d::envelope::Content::OutgoingMessage(m)) = envelope.content else {
            panic!("not an outgoing message");
        };
        assert_eq!(m.conversation, Some(contact));
        assert_eq!(m.message_id, u64::from_le_bytes([1, 2, 3, 4, 5, 6, 7, 8]));
        assert_eq!(m.thread_message_id, None);
        assert_eq!(m.created_at, 1_700_000_000_123);
        assert_eq!(m.r#type, 0x01);
        assert_eq!(m.body, b"hello");
        assert_eq!(m.nonces, vec![vec![1_u8; 24], vec![2_u8; 24]]);

        // A group message: the conversation names the group and its creator, the type is the group's.
        let group = d2d::ConversationId {
            id: Some(d2d::conversation_id::Id::Group(GroupIdentity {
                group_id: 0x1122_3344_5566_7788,
                creator_identity: "A6CRT003".to_owned(),
            })),
        };
        let content = outgoing_message(&Outgoing {
            conversation: &group,
            message_id: message_id(),
            created_at_ms: 5,
            ty: CspE2eMessageType::GroupText,
            body: &[9, 9],
            nonces: &[],
        });
        let envelope = decoded(&envelope_bytes(DEVICE, content, 0));
        let Some(d2d::envelope::Content::OutgoingMessage(m)) = envelope.content else {
            panic!("not an outgoing message");
        };
        assert_eq!(m.conversation, Some(group));
        assert_eq!(m.r#type, 0x41);
        assert_eq!(m.body, vec![9, 9]);
        assert!(
            m.nonces.is_empty(),
            "a notes group has no nonce, the list is legal empty"
        );
    }

    /// The `Sent` update decodes to its literal fields: one update, the conversation, the id, `Sent`.
    #[test]
    fn the_sent_update_decodes_to_its_fields() {
        use d2d::outgoing_message_update::{Sent, update};

        let contact = d2d::ConversationId {
            id: Some(d2d::conversation_id::Id::Contact(PEER.to_owned())),
        };
        let envelope = decoded(&envelope_bytes(
            DEVICE,
            sent_update(&contact, message_id()),
            3,
        ));
        assert_eq!(envelope.device_id, DEVICE);
        assert_eq!(envelope.protocol_version, 3);
        let Some(d2d::envelope::Content::OutgoingMessageUpdate(update_message)) = envelope.content
        else {
            panic!("not an outgoing message update");
        };
        assert_eq!(update_message.updates.len(), 1);
        let one = &update_message.updates[0];
        assert_eq!(one.conversation, Some(contact));
        assert_eq!(one.message_id, u64::from_le_bytes([1, 2, 3, 4, 5, 6, 7, 8]));
        assert_eq!(one.update, Some(update::Update::Sent(Sent {})));
    }

    /// The envelope's padding floor: message and padding are at least 64 bytes, exactly 64 when the draw is 0; a big
    /// draw stands; the draws run `0..512`.
    #[test]
    #[expect(
        deprecated,
        reason = "the padding field is read to measure the padding"
    )]
    fn the_envelope_padding_floor_is_64_bytes() {
        // The rule as literals: (message length, draw, padding length).
        for (message, drawn, pad) in [
            (0, 0, 64),
            (10, 0, 54),
            (10, 53, 54),
            (10, 54, 54),
            (10, 55, 55),
            (10, 100, 100),
            (10, 511, 511),
            (63, 0, 1),
            (63, 1, 1),
            (64, 0, 0),
            (100, 0, 0),
            (100, 300, 300),
        ] {
            assert_eq!(
                envelope_pad_length(message, drawn),
                pad,
                "message {message}, draw {drawn}"
            );
        }
        assert_eq!(ENVELOPE_PAD_DRAWS, 512);

        // On a real envelope: the update is small, so a draw of 0 is raised to the floor.
        let contact = d2d::ConversationId {
            id: Some(d2d::conversation_id::Id::Contact(PEER.to_owned())),
        };
        let content = sent_update(&contact, message_id());
        let plain = |draw: u16| {
            let bytes = envelope_bytes(DEVICE, content.clone(), draw);
            let mut envelope = decoded(&bytes);
            let padding = std::mem::take(&mut envelope.padding);
            (envelope.encoded_len(), padding)
        };
        let (message_len, padding) = plain(0);
        assert!(message_len < 64, "the update is small: {message_len}");
        assert_eq!(message_len + padding.len(), 64);
        assert!(padding.iter().all(|b| *b == 0x33), "the fill");
        let (_, padding) = plain(511);
        assert_eq!(padding.len(), 511);
        // The random draw stays in range.
        for _ in 0..200 {
            assert!(draw_envelope_pad() < 512);
            let pad = draw_pad().get();
            assert!(pad >= 1);
        }
    }

    /// The sealed envelope opens with the same Device Group Key's reflect key (P5's `open_reflected`, the inverse the
    /// follower runs) to the very bytes that were sealed, under the nonce `seal_envelope` returned, and not with
    /// another device group's key.
    #[test]
    fn a_sealed_envelope_opens_with_the_device_group_key() {
        let key = DeviceGroupKey::random();
        let contact = d2d::ConversationId {
            id: Some(d2d::conversation_id::Id::Contact(PEER.to_owned())),
        };
        let plain = envelope_bytes(DEVICE, sent_update(&contact, message_id()), 20);
        let expected = plain.to_vec();
        let (n, sealed) = seal_envelope(&key, plain).expect("seals");
        assert_eq!(
            sealed.len(),
            24 + 16 + expected.len(),
            "nonce, tag, ciphertext"
        );
        assert_eq!(&sealed[..24], &n.0, "the nonce is ahead");
        let (opened_nonce, opened) = key.open_reflected(&sealed).expect("opens");
        assert_eq!(opened_nonce, n);
        assert_eq!(opened, expected);
        assert!(DeviceGroupKey::random().open_reflected(&sealed).is_err());
    }
}
