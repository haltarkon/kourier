//! The CSP leg inside the mediator's Proxy payloads (brief §2.6): libthreema's context from the
//! stored identity, and [`CspSend`], EVERYTHING Kourier's connection may send to the chat server.
//!
//! [`CspSend::Unblock`] asks the chat server to start delivering messages: only a leader that can take
//! them sends it (TH6 L4b, once per socket, `conn`'s leader stage). Its payload is built in `src/leader/`
//! (`leader::unblock_payload`): the source check's rule 10 keeps the payload's name out of every other file
//! of `src/`, this one included. The one conversion into libthreema's payload is an exhaustive `match`.

use std::fmt;

use libthreema::common::keys::{ClientKey, PublicKey};
use libthreema::common::{CspDeviceId, DeviceCookie, MessageFlags, MessageId, ThreemaId};
use libthreema::csp::CspProtocolContext;
use libthreema::csp::CspProtocolContextInit;
use libthreema::csp::payload::{
    ConnectionIdleTimeout, EchoPayload, MessageAck, MessageWithMetadataBox, OutgoingPayload,
};

use super::lines::Out;
use crate::ServerParams;
use crate::send::queue::SendOrigin;
use crate::store::Identity;

/// Where the fields of a stored `message-with-metadata-box` are (`csp.struct.yml`): the sender's ID, the message id, the
/// flags.
const BOX_SENDER: std::ops::Range<usize> = 0..8;
const BOX_MESSAGE_ID: std::ops::Range<usize> = 16..24;
const BOX_FLAGS: usize = 28;

/// What the connection sends to the chat server. `Debug` prints lengths: a box holds a receiver's ID in the clear.
#[derive(Clone, PartialEq, Eq)]
pub(crate) enum CspSend {
    /// A keep-alive probe; the payload is the time it went out.
    EchoRequest(Vec<u8>),
    /// The answer to the server's echo request, its payload unchanged.
    EchoResponse(Vec<u8>),
    /// The chat server's idle timeout, in SECONDS (the spec's unit; libthreema's doc says
    /// minutes).
    SetConnectionIdleTimeout(u16),
    /// One sealed `message-with-metadata-box`, written as it is stored (TH6a). Plain values: libthreema's type is
    /// `Clone` only under its own tests, and only `bytes` goes on the wire.
    Box {
        /// The sender's ID, as the box says.
        sender: ThreemaId,
        /// The message id, as the box says.
        message_id: u64,
        /// The flags, as the box says.
        flags: u8,
        /// The whole box.
        bytes: Vec<u8>,
    },
    /// TH6 L4b: the leader asks the chat server to deliver (once per socket, after Dry was counted).
    Unblock,
    /// TH6 L4b: the `message-ack` of an incoming message, after the zone that makes it true.
    MessageAck {
        /// The message's sender.
        sender: ThreemaId,
        /// The message id.
        message_id: u64,
    },
}

impl CspSend {
    /// The frame of a stored box, its head read for libthreema's payload; `None` for bytes too short or with no ID at
    /// the sender's place (the schema's size CHECK and the seal rule that out).
    pub(crate) fn boxed(bytes: Vec<u8>) -> Option<Self> {
        let sender = ThreemaId::try_from(bytes.get(BOX_SENDER)?).ok()?;
        let message_id = u64::from_le_bytes(bytes.get(BOX_MESSAGE_ID)?.try_into().ok()?);
        let flags = *bytes.get(BOX_FLAGS)?;
        Some(Self::Box {
            sender,
            message_id,
            flags,
            bytes,
        })
    }

    /// What the count line calls this frame (TH6 C1, [`super::lines`]): a box by the `origin` of its row (TR's D5), `other`
    /// when none is known. Exhaustive: a new value must say how it is counted.
    pub(crate) fn counted(&self, origin: Option<SendOrigin>) -> Out {
        match self {
            Self::EchoRequest(_) | Self::EchoResponse(_) => Out::Echo,
            Self::SetConnectionIdleTimeout(_) => Out::Idle,
            Self::Box { .. } => origin.map_or(Out::Other, Out::Box),
            Self::Unblock => Out::Unblock,
            Self::MessageAck { .. } => Out::MessageAck,
        }
    }

    /// libthreema's payload. Exhaustive: a new value must say what it sends.
    pub(crate) fn payload(self) -> OutgoingPayload {
        match self {
            Self::EchoRequest(data) => OutgoingPayload::EchoRequest(EchoPayload(data)),
            Self::EchoResponse(data) => OutgoingPayload::EchoResponse(EchoPayload(data)),
            Self::SetConnectionIdleTimeout(seconds) => {
                OutgoingPayload::SetConnectionIdleTimeout(ConnectionIdleTimeout(seconds))
            }
            Self::Box {
                sender,
                message_id,
                flags,
                bytes,
            } => OutgoingPayload::MessageWithMetadataBox(MessageWithMetadataBox {
                sender_identity: sender,
                message_id: MessageId(message_id),
                flags: MessageFlags(flags),
                bytes,
            }),
            Self::Unblock => crate::leader::unblock_payload(),
            Self::MessageAck { sender, message_id } => OutgoingPayload::MessageAck(MessageAck {
                sender_identity: sender,
                message_id: MessageId(message_id),
            }),
        }
    }
}

impl fmt::Debug for CspSend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EchoRequest(data) => f.debug_tuple("EchoRequest").field(&data.len()).finish(),
            Self::EchoResponse(data) => f.debug_tuple("EchoResponse").field(&data.len()).finish(),
            Self::SetConnectionIdleTimeout(seconds) => f
                .debug_tuple("SetConnectionIdleTimeout")
                .field(seconds)
                .finish(),
            Self::Box { flags, bytes, .. } => f
                .debug_struct("Box")
                .field("flags", flags)
                .field("len", &bytes.len())
                .finish_non_exhaustive(),
            Self::Unblock => f.write_str("Unblock"),
            Self::MessageAck { .. } => f.write_str("MessageAck"),
        }
    }
}

/// The CSP context of one connect: the parts' chat-server keys (primary first), the identity's ID
/// and CK, the client info, the device cookie and the CSP device id. An error names the field.
pub(crate) fn context(
    identity: &Identity,
    server: &ServerParams,
) -> Result<CspProtocolContext, &'static str> {
    CspProtocolContext::try_from(CspProtocolContextInit {
        permanent_server_keys: server
            .chat_server_keys
            .iter()
            .copied()
            .map(PublicKey::from)
            .collect(),
        user_identity: ThreemaId::try_from(identity.threema_id.as_str())
            .map_err(|_| "the Threema ID")?,
        client_key: ClientKey::from(*identity.client_key),
        client_info: super::d2m::client_info(),
        device_cookie: Some(DeviceCookie(*identity.device_cookie)),
        csp_device_id: Some(CspDeviceId(identity.csp_device_id)),
    })
    .map_err(|_| "the chat server keys")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A box's head, by the spec's offsets: the sender, the receiver, the id's wire bytes, `created-at`, the flags.
    fn a_box() -> Vec<u8> {
        let mut bytes = b"KOURIER3PEER0001".to_vec();
        bytes.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        bytes.extend_from_slice(&[0; 4]);
        bytes.push(0x21);
        bytes.extend_from_slice(&[0; 59]);
        bytes
    }

    /// L11: the head is read from the stored bytes, the bytes go whole, and libthreema's payload holds them.
    #[test]
    fn a_stored_box_is_its_head_and_its_bytes_as_they_are() {
        let CspSend::Box {
            sender,
            message_id,
            flags,
            ref bytes,
        } = CspSend::boxed(a_box()).expect("a box")
        else {
            panic!("not a box");
        };
        assert_eq!(sender.as_str(), "KOURIER3");
        assert_eq!(message_id, 0x0807_0605_0403_0201);
        assert_eq!(flags, 0x21);
        assert!(*bytes == a_box(), "the bytes changed");
        let OutgoingPayload::MessageWithMetadataBox(payload) =
            CspSend::boxed(a_box()).expect("a box").payload()
        else {
            panic!("not libthreema's box");
        };
        assert_eq!(payload.sender_identity.as_str(), "KOURIER3");
        assert_eq!(payload.message_id.0, 0x0807_0605_0403_0201);
        assert_eq!(payload.flags.0, 0x21);
        assert!(payload.bytes == a_box(), "the bytes changed");
    }

    #[test]
    fn bytes_that_hold_no_box_head_are_no_frame() {
        assert!(CspSend::boxed(vec![0x4b; 28]).is_none(), "under the flags");
        let mut no_id = a_box();
        no_id[0] = b'k';
        assert!(
            CspSend::boxed(no_id).is_none(),
            "no ID at the sender's place"
        );
    }

    /// L11: a box holds a receiver's ID in the clear at offset 8: `Debug` prints neither ID nor byte.
    #[test]
    fn debug_of_a_box_prints_its_flags_and_its_length() {
        let printed = format!("{:?}", CspSend::boxed(a_box()).expect("a box"));
        assert!(!printed.contains("PEER"), "an ID in {printed}");
        assert!(!printed.contains("KOURIER"), "an ID in {printed}");
        assert!(printed.contains("len: 88"), "the length in {printed}");
    }
}
