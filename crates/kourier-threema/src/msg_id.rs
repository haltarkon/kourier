//! The text form of a Threema message id, ONE for the whole crate (critic M4): libthreema's own
//! (`MessageId`'s `Display`), the eight wire bytes in order as 16 lowercase hex digits. An own
//! message, its reflection and its receipts carry the same text only if every path goes through
//! [`format`] and [`parse`]; the ids land in core.db rows, so a later change would break a kept
//! profile.

use libthreema::common::MessageId;

/// The id's text: libthreema's `Display` (the wire bytes `to_bytes()` in order, lowercase hex).
pub(crate) fn format(id: MessageId) -> String {
    id.to_string()
}

/// The id of a text [`format`] made: exactly 16 lowercase hex digits, the wire bytes in order.
/// Anything else is `None` (upper case too: one id has one text).
pub(crate) fn parse(text: &str) -> Option<MessageId> {
    let digits = text.as_bytes();
    if digits.len() != 2 * MessageId::LENGTH {
        return None;
    }
    let nibble = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    };
    let mut bytes = [0u8; MessageId::LENGTH];
    let (pairs, _) = digits.as_chunks::<2>();
    for (byte, [high, low]) in bytes.iter_mut().zip(pairs) {
        *byte = (nibble(*high)? << 4) | nibble(*low)?;
    }
    Some(MessageId::from(bytes))
}

/// A new random id (libthreema's own generator) in its text form.
pub(crate) fn random() -> String {
    format(MessageId::random())
}

#[cfg(test)]
mod tests {
    use super::*;

    const WIRE: [u8; 8] = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08];

    #[test]
    fn the_text_is_the_wire_bytes_in_order() {
        let id = MessageId::from(WIRE);
        assert_eq!(id.to_bytes(), WIRE, "libthreema's wire form changed");
        assert_eq!(format(id), "0102030405060708");
        let back = parse("0102030405060708").expect("a valid id");
        assert_eq!(back.to_bytes(), WIRE);
        // A byte above 0x7f and a letter digit, both directions.
        let high = MessageId::from([0xff, 0x00, 0xab, 0x10, 0x00, 0x00, 0x00, 0x9c]);
        assert_eq!(format(high), "ff00ab100000009c");
        assert_eq!(parse("ff00ab100000009c"), Some(high));
    }

    #[test]
    fn only_the_one_text_form_parses() {
        for bad in [
            "",
            "010203040506070",
            "01020304050607080",
            "0102030405060G08",
            "01020304050607AB",
            " 102030405060708",
            "0x02030405060708",
        ] {
            assert_eq!(
                parse(bad),
                None,
                "a malformed id parsed (case {})",
                bad.len()
            );
        }
    }

    #[test]
    fn a_random_id_goes_through_the_one_form() {
        let text = random();
        let id = parse(&text).expect("a random id parses");
        assert_eq!(format(id), text);
    }
}
