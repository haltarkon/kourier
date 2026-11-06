//! THE LEADER'S REFLECT OF AN INCOMING MESSAGE (TH6 L4b; brief §2.1, §2.7): the `d2d.Envelope { incoming_message }`
//! (`TP/md-d2d.proto:247-292`: sender, message id, created-at in ms, the OUTER type, the UNPADDED body, the CSP nonce;
//! protocol V0.3, the own device id, random padding as the send core's envelope), SEALED ONCE with the Device Group
//! Reflect Key under a D2D nonce the store has not seen (redrawn on a hit, up to [`NONCE_TRIES`] times, the send
//! core's PREPARE rule). ZONE RECEIVE keeps the sealed bytes in the `pending` row, and every later reflect of that row
//! (the same socket after a lost frame, a later socket at step 0) writes THOSE bytes: NEVER seal again under a stored
//! nonce (TH6a's sentence: the padding is random, so a second seal under the same nonce would reuse the keystream).
//!
//! The seal itself is the send core's one function (`send::encode::seal_envelope`): this module names no P5 box, so
//! rule 14 keeps every seal out of `src/leader/` as it stands.

use libthreema::common::Nonce;
use libthreema::protobuf::d2d;
use zeroize::Zeroizing;

use crate::error::{Result, StoreError};
use crate::reflect::Keys;
use crate::send::encode;

/// How often a D2D nonce is drawn again when the store has it (the send core's number).
const NONCE_TRIES: u32 = 8;
/// The most bytes of a sealed reflect (`csp_inbound.reflect_env`'s CHECK; the send core's envelope bound).
const SEALED_MAX: usize = 65_516;

const OP_SEAL: &str = "seal a received message's reflect failed";
const OP_NONCE: &str = "no unused nonce for a received message's reflect";
const OP_TOO_LARGE: &str = "a received message's reflect is too large";

/// What the leader reflects of one incoming message: the fields of `IncomingMessage`.
pub(crate) struct Incoming<'a> {
    /// The sender's Threema ID (the clear head's).
    pub(crate) sender: &'a str,
    /// The message id (the clear head's).
    pub(crate) message_id: u64,
    /// T-L3: the metadata's time (the head's seconds x 1000 without metadata), clamped to now.
    pub(crate) created_at_ms: u64,
    /// The OUTER type (footnote 2).
    pub(crate) outer_type: i32,
    /// The UNPADDED `padded-data`.
    pub(crate) body: &'a [u8],
    /// The CSP nonce of the message.
    pub(crate) nonce: &'a Nonce,
}

/// The plaintext `d2d.Envelope { incoming_message }` of `message` from `device`, padded as the send core pads (what
/// ZONE RECEIVE maps and what is sealed). `draw` is the padding's draw ([`encode::draw_envelope_pad`]).
pub(crate) fn plaintext(device: u64, message: &Incoming<'_>, draw: u16) -> Zeroizing<Vec<u8>> {
    let content = d2d::envelope::Content::IncomingMessage(d2d::IncomingMessage {
        sender_identity: message.sender.to_owned(),
        message_id: message.message_id,
        created_at: message.created_at_ms,
        r#type: message.outer_type,
        body: message.body.to_vec(),
        nonce: message.nonce.0.to_vec(),
    });
    encode::envelope_bytes(device, content, draw)
}

/// `plaintext` sealed once under a D2D nonce that `used` (the store's `d2d_nonce`, hashed) has not seen: the sealed
/// bytes and the nonce's hash. `Err` is the content's (`is_content()`): no fresh nonce in [`NONCE_TRIES`] draws, a seal
/// the cipher refused, or bytes over the column's bound; the caller holds that message (C6).
pub(crate) fn seal_once(
    keys: &Keys,
    plaintext: &Zeroizing<Vec<u8>>,
    used: &dyn Fn(&[u8; 32]) -> Result<bool>,
) -> Result<(Vec<u8>, [u8; 32])> {
    for _ in 0..NONCE_TRIES {
        let (nonce, sealed) = encode::seal_envelope(keys.dgk(), plaintext.clone())
            .map_err(|_| StoreError::Failed(OP_SEAL))?;
        if sealed.len() > SEALED_MAX {
            return Err(StoreError::Failed(OP_TOO_LARGE));
        }
        let hash = keys.hash(&nonce);
        if !used(&hash)? {
            return Ok((sealed, hash));
        }
    }
    Err(StoreError::Failed(OP_NONCE))
}

/// The EPHEMERAL reflect of an incoming typing indicator: sealed with a fresh nonce that is never stored (no zone, no
/// nonce, no ack; the mediator never acks an ephemeral reflect).
pub(crate) fn seal_ephemeral(keys: &Keys, plaintext: &Zeroizing<Vec<u8>>) -> Result<Vec<u8>> {
    encode::seal_envelope(keys.dgk(), plaintext.clone())
        .map(|(_nonce, sealed)| sealed)
        .map_err(|_| StoreError::Failed(OP_SEAL))
}

#[cfg(test)]
mod tests {
    use libthreema::common::ThreemaId;
    use libthreema::common::keys::DeviceGroupKey;
    use prost::Message as _;

    use super::*;

    fn keys() -> Keys {
        Keys::for_tests(
            DeviceGroupKey::from([3; 32]),
            ThreemaId::try_from("KOURIER1").expect("id"),
        )
    }

    /// The plaintext carries every field of the message as given, and the seal opens to exactly those bytes; a second
    /// seal draws another nonce (the caller keeps the first: sealed ONCE).
    #[test]
    fn the_reflect_carries_the_message_and_opens_to_the_plaintext() {
        let keys = keys();
        let nonce = Nonce([9; 24]);
        let message = Incoming {
            sender: "PEERAAAA",
            message_id: 0x1122_3344_5566_7788,
            created_at_ms: 1_700_000_000_123,
            outer_type: 0x01,
            body: b"synthetic",
            nonce: &nonce,
        };
        let plain = plaintext(7, &message, 0);
        let envelope = d2d::Envelope::decode(plain.as_slice()).expect("an envelope");
        assert_eq!(envelope.device_id, 7);
        let Some(d2d::envelope::Content::IncomingMessage(m)) = envelope.content else {
            panic!("not an incoming message");
        };
        assert_eq!(
            (
                m.sender_identity.as_str(),
                m.message_id,
                m.created_at,
                m.r#type
            ),
            ("PEERAAAA", 0x1122_3344_5566_7788, 1_700_000_000_123, 0x01)
        );
        assert_eq!(
            (m.body.as_slice(), m.nonce.as_slice()),
            (&b"synthetic"[..], &[9; 24][..])
        );
        let (sealed, hash) = seal_once(&keys, &plain, &|_| Ok(false)).expect("sealed");
        let (opened_nonce, opened) = keys.dgk().open_reflected(&sealed).expect("opens");
        assert_eq!(opened.as_slice(), plain.as_slice());
        assert_eq!(keys.hash(&opened_nonce), hash);
        let (again, _) = seal_once(&keys, &plain, &|_| Ok(false)).expect("sealed");
        assert_ne!(
            sealed[..24],
            again[..24],
            "a second seal drew the same nonce"
        );
    }

    /// A nonce the store has is drawn again; eight hits in a row fail by the content.
    #[test]
    fn a_used_nonce_is_drawn_again_and_eight_hits_fail_by_the_content() {
        let keys = keys();
        let nonce = Nonce([1; 24]);
        let message = Incoming {
            sender: "PEERAAAA",
            message_id: 1,
            created_at_ms: 1,
            outer_type: 0x01,
            body: b"x",
            nonce: &nonce,
        };
        let plain = plaintext(7, &message, 0);
        let hits = std::cell::Cell::new(0);
        let once = |_: &[u8; 32]| -> Result<bool> {
            hits.set(hits.get() + 1);
            Ok(hits.get() == 1)
        };
        assert!(seal_once(&keys, &plain, &once).is_ok());
        assert_eq!(hits.get(), 2);
        let error = seal_once(&keys, &plain, &|_| Ok(true)).expect_err("no nonce");
        assert!(error.is_content(), "{error:?}");
    }
}
