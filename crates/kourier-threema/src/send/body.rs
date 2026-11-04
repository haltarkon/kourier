//! THE BODIES A SEND WRITES (TH6a S2): the "unpadded `csp.e2e.container.padded-data`" of a message, which is what a
//! reflected `OutgoingMessage.body` carries and what the container pads and the box seals. Pure writers over bytes;
//! the parsers of the same forms are `reflect::body` (its tests read these writers back).
//!
//! TH6a writes two: a TEXT (`e2e.text`: "UTF-8 encoded text", nothing else, `csp.struct.yml`) and the
//! `group-member-container` that wraps every group message a member sends (`csp.struct.yml`: the creator's 8-byte ID,
//! the group id as `u64-le`, then the inner message struct). The receipt's body and the `group-creator-container`
//! are written by the steps that need them (TH6, TH7); a writer with no caller fails the build (L9).
//!
//! A quote (`> quote #<id>\n\n<comment>`) and a mention (`@[ID]`) are the text's own content, written by whoever
//! builds the text; the pinned spec describes neither (`reflect::body` states the forms).
//!
//! Nothing here logs or prints a body.

use libthreema::common::ThreemaId;

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
}
