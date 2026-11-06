//! THE BODIES A SEND WRITES (TH6a S2): the "unpadded `csp.e2e.container.padded-data`" of a message, which is what a
//! reflected `OutgoingMessage.body` carries and what the container pads and the box seals. Pure writers over bytes;
//! the parsers of the same forms are `reflect::body` (its tests read these writers back).
//!
//! TH6a writes two: a TEXT (`e2e.text`: "UTF-8 encoded text", nothing else, `csp.struct.yml`) and the
//! `group-member-container` that wraps every group message a member sends (`csp.struct.yml`: the creator's 8-byte ID,
//! the group id as `u64-le`, then the inner message struct). TH6 (L3) adds what the leader's jobs send: the
//! `delivery-receipt` (status, then the ids), the `group-creator-container` (the group id as `u64-le`, then the inner
//! struct), and the creator's answers `group-setup` (the members' 8-byte IDs, nothing else) and `group-name` (the
//! name's UTF-8 bytes). `group-delete-profile-picture`, `group-sync-request` and `group-leave` are EMPTY structs: their
//! body is the container around no bytes. The fake's builders (`tests/support/mock/body.rs`) are the oracle, byte
//! for byte (`tests/leader.rs`, through the doc-hidden door `store::write_body`).
//!
//! A quote (`> quote #<id>\n\n<comment>`) and a mention (`@[ID]`) are the text's own content, written by whoever
//! builds the text; the pinned spec describes neither (`reflect::body` states the forms).
//!
//! Nothing here logs or prints a body.

use libthreema::common::{MessageId, ThreemaId};

/// The body of `e2e.text`: the text's UTF-8 bytes.
pub(crate) fn text(text: &str) -> Vec<u8> {
    text.as_bytes().to_vec()
}

/// `e2e.group-member-container`: `creator` (the 8 ASCII bytes of the group creator's Threema ID, which
/// [`ThreemaId`] already checked), `group_id` (`u64-le`), then `inner`, the message struct the type names.
pub(crate) fn group_member_container(creator: ThreemaId, group_id: u64, inner: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(ThreemaId::LENGTH + 8 + inner.len());
    out.extend_from_slice(&creator.to_bytes());
    out.extend_from_slice(&group_id.to_le_bytes());
    out.extend_from_slice(inner);
    out
}

/// `e2e.group-creator-container`: `group-id` (`u64-le`), then `inner`, the message struct the type names. The wrapper of
/// the special group messages the creator sends (setup, name, the pictures); the creator is the sender.
pub(crate) fn group_creator_container(group_id: u64, inner: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + inner.len());
    out.extend_from_slice(&group_id.to_le_bytes());
    out.extend_from_slice(inner);
    out
}

/// `e2e.delivery-receipt`: `status` (1 received, 2 read, 3 and 4 the deprecated reactions) then one or more
/// `message-id`s, each `u64-le` (the 8 wire bytes of [`MessageId`]). The leader's automatic receipt is status 1 with up
/// to 30 ids of one sender (C3).
pub(crate) fn delivery_receipt(status: u8, ids: &[MessageId]) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + ids.len() * 8);
    out.push(status);
    for id in ids {
        out.extend_from_slice(&id.to_bytes());
    }
    out
}

/// `e2e.group-setup`: `members`, the 8 ASCII bytes of each member's Threema ID (`b8[]`), the creator and nothing else
/// in front or behind. EMPTY for the setup that tells a sender it is not (or no longer) a member.
pub(crate) fn group_setup(members: &[ThreemaId]) -> Vec<u8> {
    let mut out = Vec::with_capacity(members.len() * ThreemaId::LENGTH);
    for member in members {
        out.extend_from_slice(&member.to_bytes());
    }
    out
}

/// `e2e.group-name`: the name's UTF-8 bytes (`b*`), nothing else.
pub(crate) fn group_name(name: &str) -> Vec<u8> {
    name.as_bytes().to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The text body is the text's bytes and nothing around them: no length, no terminator, no padding (the container
    /// pads, the box seals).
    #[test]
    fn a_text_is_its_utf8_bytes() {
        assert_eq!(text("hi"), vec![0x68, 0x69]);
        assert_eq!(text(""), Vec::<u8>::new());
        // Multi-byte characters stay as UTF-8, a line break stays as it is.
        assert_eq!(
            text("é\n😀"),
            vec![0xc3, 0xa9, 0x0a, 0xf0, 0x9f, 0x98, 0x80]
        );
    }

    /// `group-member-container` byte by byte: the creator's ASCII, the group id little-endian, the inner bytes.
    #[test]
    fn a_member_container_is_creator_then_group_id_le_then_inner() {
        let creator = ThreemaId::try_from("A6CRT003").expect("a Threema ID");
        let out = group_member_container(creator, 0x0102_0304_0506_0708, &[0xaa, 0xbb, 0xcc]);
        assert_eq!(
            out,
            vec![
                0x41, 0x36, 0x43, 0x52, 0x54, 0x30, 0x30, 0x33, // "A6CRT003"
                0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01, // the group id, little-endian
                0xaa, 0xbb, 0xcc,
            ]
        );
        // An empty inner message is legal for the container (the type decides what it holds); a Gateway creator
        // keeps its `*`.
        let gateway = ThreemaId::try_from("*GATEWAY").expect("a Gateway ID");
        let out = group_member_container(gateway, u64::MAX, &[]);
        assert_eq!(out.len(), 16);
        assert_eq!(&out[..8], b"*GATEWAY");
        assert_eq!(&out[8..], &[0xff; 8]);
    }

    /// The writers of TH6 byte by byte (the spec's fields in order): the creator container, a receipt of two ids, a
    /// setup of two members and the empty one, a name, and the empty structs inside their containers.
    #[test]
    fn the_leaders_writers_put_every_field_in_its_place() {
        assert_eq!(
            group_creator_container(0x0102_0304_0506_0708, &[0xaa]),
            vec![0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01, 0xaa]
        );
        assert_eq!(
            group_creator_container(5, &[]),
            5_u64.to_le_bytes().to_vec()
        );
        let ids = [
            MessageId(u64::from_le_bytes([1, 2, 3, 4, 5, 6, 7, 8])),
            MessageId(u64::from_le_bytes([0xf0; 8])),
        ];
        assert_eq!(
            delivery_receipt(1, &ids),
            vec![
                0x01, // received
                1, 2, 3, 4, 5, 6, 7, 8, // the first id in wire order
                0xf0, 0xf0, 0xf0, 0xf0, 0xf0, 0xf0, 0xf0, 0xf0,
            ]
        );
        assert_eq!(delivery_receipt(2, &[]), vec![0x02]);
        let a = ThreemaId::try_from("A6MEM001").expect("an ID");
        let b = ThreemaId::try_from("A6MEM002").expect("an ID");
        assert_eq!(group_setup(&[a, b]), b"A6MEM001A6MEM002".to_vec());
        assert!(group_setup(&[]).is_empty(), "the empty setup");
        assert_eq!(group_name("Rhein ☁"), "Rhein ☁".as_bytes().to_vec());
        assert!(group_name("").is_empty());
        // The empty structs: the container and nothing more.
        let creator = ThreemaId::try_from("A6CRT003").expect("an ID");
        assert_eq!(group_member_container(creator, 9, &[]).len(), 16);
        assert_eq!(group_creator_container(9, &[]).len(), 8);
    }
}
