//! The Storage Service wire editor (A10b): changes a few fields of a stored record's bytes and
//! leaves every other byte alone.
//!
//! A record the phone wrote carries fields this build does not know (a newer client's), and
//! sub-messages that hold unknown fields of their own. Decoding it into the prost struct and
//! encoding that again would drop all of them, and the phone would lose settings it never asked
//! us to touch. So a record is NEVER re-encoded from the prost struct: it is walked as
//! `(number, wire type, span)` ([`fields`]), and an edit builds a new byte string out of the
//! old one, replacing, inserting or removing whole fields ([`set_varint`], [`set_bytes`],
//! [`edit_message`], [`retain_repeated`], [`push_message`]). Reading a record with prost stays
//! right (the merges do); only writing is done here.
//!
//! Rules every function keeps:
//! - The whole message is walked and checked BEFORE anything is built, so malformed input (a
//!   truncated tag or varint, a length past the end, a group or reserved wire type, field number
//!   0) is an `Err` and never a partial edit. Nothing panics on any input.
//! - A field that is not edited is copied as it was written, including a non-canonical varint
//!   or a repeated occurrence of an unknown field.
//! - The value 0 (or empty bytes) is the field ABSENT, as every client writes proto3 defaults;
//!   repeated occurrences of an edited scalar are dropped (the last one would win in a decoder,
//!   so keeping a stale one beside ours would be wrong).
//! - A field that was absent goes where an encoder would put it: after the last field whose
//!   number is not greater (the message stays in ascending order when it was), so a repeated
//!   field's new entry comes after the existing ones.
//! - The errors carry no bytes, only a kind: they are log-safe (architecture §9.1).
//!
//! A NEW pin entry has the form of the phone's own (BQ5, his Android phone): `PinnedConversation
//! { contact { serviceIdBinary = the 16-byte ACI } }` and nothing else (no string `serviceId`),
//! or `{ groupMasterKey }` ([`contact_pin_entry`], [`group_pin_entry`]). Note to Self is pinned
//! by OUR ACI.

use std::ops::Range;

use libsignal_core::Aci;

use crate::error::Error;

/// The field numbers the write engine edits (`protos/SignalStorage.proto`; a test proves each
/// against the generated types).
pub(crate) mod num {
    /// `StorageRecord`'s oneof: `contact`.
    pub(crate) const RECORD_CONTACT: u32 = 1;
    /// `StorageRecord`'s oneof: `groupV2`.
    pub(crate) const RECORD_GROUP_V2: u32 = 3;
    /// `StorageRecord`'s oneof: `account`.
    pub(crate) const RECORD_ACCOUNT: u32 = 4;

    /// `ContactRecord.identityKey` (never written: DEVIATION L4; the tests pin the number).
    #[cfg(test)]
    pub(crate) const CONTACT_IDENTITY_KEY: u32 = 4;
    /// `ContactRecord.identityState`.
    pub(crate) const CONTACT_IDENTITY_STATE: u32 = 5;
    /// `ContactRecord.archived`.
    pub(crate) const CONTACT_ARCHIVED: u32 = 11;
    /// `ContactRecord.markedUnread`.
    pub(crate) const CONTACT_MARKED_UNREAD: u32 = 12;
    /// `ContactRecord.mutedUntilTimestamp`.
    pub(crate) const CONTACT_MUTED_UNTIL: u32 = 13;

    /// `GroupV2Record.archived`.
    pub(crate) const GROUP_ARCHIVED: u32 = 4;
    /// `GroupV2Record.markedUnread`.
    pub(crate) const GROUP_MARKED_UNREAD: u32 = 5;
    /// `GroupV2Record.mutedUntilTimestamp`.
    pub(crate) const GROUP_MUTED_UNTIL: u32 = 6;

    /// `AccountRecord.noteToSelfArchived`.
    pub(crate) const ACCOUNT_NOTE_TO_SELF_ARCHIVED: u32 = 5;
    /// `AccountRecord.noteToSelfMarkedUnread`.
    pub(crate) const ACCOUNT_NOTE_TO_SELF_MARKED_UNREAD: u32 = 10;
    /// `AccountRecord.pinnedConversations` (repeated).
    pub(crate) const ACCOUNT_PINNED: u32 = 14;
}

/// The largest field number a tag can carry (29 bits).
const MAX_FIELD: u32 = (1 << 29) - 1;

/// Why a message could not be walked or edited. No variant holds a byte of the message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum WireError {
    /// A tag, a varint or a fixed-size value ends before it is complete.
    #[error("a field is cut off by the end of the message")]
    Truncated,
    /// A varint longer than ten bytes or larger than 64 bits.
    #[error("a varint is too long")]
    BadVarint,
    /// Field number 0, or a tag that does not fit 32 bits.
    #[error("a field number is zero or too large")]
    BadTag,
    /// A group (wire types 3, 4) or a reserved wire type (6, 7).
    #[error("a group or reserved wire type")]
    UnsupportedType,
    /// A length that runs past the end of the message.
    #[error("a length runs past the end of the message")]
    BadLength,
    /// The field to edit is there with another wire type than the edit needs.
    #[error("a field has another wire type than the edit needs")]
    WrongType,
    /// A singular message field occurs more than once.
    #[error("a singular message field occurs more than once")]
    Repeated,
    /// The message field to edit is not there.
    #[error("the message field to edit is missing")]
    Missing,
}

impl From<WireError> for Error {
    fn from(_: WireError) -> Self {
        Self::Decode("a Storage Service record (wire form)")
    }
}

/// The wire types a field can have here (groups are refused).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WireType {
    /// 0: a varint.
    Varint,
    /// 1: eight bytes.
    Fixed64,
    /// 2: a length, then that many bytes.
    Len,
    /// 5: four bytes.
    Fixed32,
}

impl WireType {
    /// The 3-bit code in a tag.
    fn code(self) -> u64 {
        match self {
            Self::Varint => 0,
            Self::Fixed64 => 1,
            Self::Len => 2,
            Self::Fixed32 => 5,
        }
    }
}

/// One field of a message, located and not decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Field {
    /// The field number.
    pub(crate) number: u32,
    /// How the value is encoded.
    pub(crate) wire_type: WireType,
    /// The whole field: tag, length prefix (if any) and value.
    pub(crate) span: Range<usize>,
    /// Where the tag ends: the first byte of the length prefix or of the value.
    pub(crate) tag_end: usize,
    /// The value's bytes: the varint, the 4 or 8 fixed bytes, or the content of a
    /// length-delimited field (after its length prefix).
    pub(crate) value: Range<usize>,
}

/// The varint at `pos` and the position after it (`Truncated` when the message ends first,
/// `BadVarint` when it is longer than ten bytes or over 64 bits).
fn read_varint(buf: &[u8], mut pos: usize) -> Result<(u64, usize), WireError> {
    let mut value = 0u64;
    for i in 0..10u32 {
        let byte = *buf.get(pos).ok_or(WireError::Truncated)?;
        pos += 1;
        if i == 9 && byte > 1 {
            return Err(WireError::BadVarint);
        }
        value |= u64::from(byte & 0x7f) << (7 * i);
        if byte & 0x80 == 0 {
            return Ok((value, pos));
        }
    }
    Err(WireError::BadVarint)
}

/// Appends `value` as a (canonical) varint.
fn put_varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push((value & 0x7f) as u8 | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

/// Appends a tag.
fn put_tag(out: &mut Vec<u8>, number: u32, wire_type: WireType) {
    put_varint(out, u64::from(number) << 3 | wire_type.code());
}

/// Appends a length-delimited field.
fn put_len_field(out: &mut Vec<u8>, number: u32, payload: &[u8]) {
    put_tag(out, number, WireType::Len);
    put_varint(out, payload.len() as u64);
    out.extend_from_slice(payload);
}

/// Walks `msg` field by field and checks it all: the fields in the order written, each with
/// its place. Nothing is decoded. `Err` when any byte of the message is malformed, so a caller
/// that has the list knows the rest of the message is sound too.
pub(crate) fn fields(msg: &[u8]) -> Result<Vec<Field>, WireError> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < msg.len() {
        let start = pos;
        let (key, after_tag) = read_varint(msg, pos)?;
        let key = u32::try_from(key).map_err(|_| WireError::BadTag)?;
        let number = key >> 3;
        if number == 0 || number > MAX_FIELD {
            return Err(WireError::BadTag);
        }
        let (wire_type, value, end) = match key & 7 {
            0 => {
                let (_, end) = read_varint(msg, after_tag)?;
                (WireType::Varint, after_tag..end, end)
            }
            1 => {
                let end = fixed_end(msg, after_tag, 8)?;
                (WireType::Fixed64, after_tag..end, end)
            }
            2 => {
                let (len, body) = read_varint(msg, after_tag)?;
                let end = usize::try_from(len)
                    .ok()
                    .and_then(|len| body.checked_add(len))
                    .filter(|end| *end <= msg.len())
                    .ok_or(WireError::BadLength)?;
                (WireType::Len, body..end, end)
            }
            5 => {
                let end = fixed_end(msg, after_tag, 4)?;
                (WireType::Fixed32, after_tag..end, end)
            }
            _ => return Err(WireError::UnsupportedType),
        };
        out.push(Field {
            number,
            wire_type,
            span: start..end,
            tag_end: after_tag,
            value,
        });
        pos = end;
    }
    Ok(out)
}

/// The end of a fixed-size value of `size` bytes at `pos`.
fn fixed_end(msg: &[u8], pos: usize, size: usize) -> Result<usize, WireError> {
    pos.checked_add(size)
        .filter(|end| *end <= msg.len())
        .ok_or(WireError::Truncated)
}

/// `msg` with each edit applied: a range replaced by bytes (an empty range inserts, empty bytes
/// remove). The ranges come from [`fields`] of `msg`, ascending and apart.
fn rebuild(msg: &[u8], edits: &[(Range<usize>, Vec<u8>)]) -> Vec<u8> {
    let mut out = Vec::with_capacity(msg.len() + 16);
    let mut at = 0;
    for (range, with) in edits {
        debug_assert!(at <= range.start && range.start <= range.end);
        out.extend_from_slice(&msg[at..range.start]);
        out.extend_from_slice(with);
        at = range.end;
    }
    out.extend_from_slice(&msg[at..]);
    out
}

/// Where a new occurrence of field `number` goes: after the last field whose number is not
/// greater (so after every existing occurrence of `number`), else at the start.
fn insert_position(all: &[Field], number: u32) -> usize {
    all.iter()
        .rev()
        .find(|f| f.number <= number)
        .map_or(0, |f| f.span.end)
}

/// The occurrences of `number`, all of the wire type `wire_type` (else `WrongType`).
fn occurrences(all: &[Field], number: u32, wire_type: WireType) -> Result<Vec<&Field>, WireError> {
    let found: Vec<&Field> = all.iter().filter(|f| f.number == number).collect();
    if found.iter().any(|f| f.wire_type != wire_type) {
        return Err(WireError::WrongType);
    }
    Ok(found)
}

/// Removes every occurrence in `rest` (each whole field).
fn remove_all(edits: &mut Vec<(Range<usize>, Vec<u8>)>, rest: &[&Field]) {
    for field in rest {
        edits.push((field.span.clone(), Vec::new()));
    }
}

/// Sets the varint field `number` of `msg` to `value`. The first occurrence is replaced in
/// place (only its value bytes change; it is left as written when the value is already
/// `value`), repeats are dropped. `value == 0` removes the field. An absent field is
/// inserted where an encoder puts it.
pub(crate) fn set_varint(msg: &[u8], number: u32, value: u64) -> Result<Vec<u8>, WireError> {
    let all = fields(msg)?;
    let found = occurrences(&all, number, WireType::Varint)?;
    let mut edits = Vec::new();
    match found.split_first() {
        None if value == 0 => {}
        None => {
            let mut new = Vec::new();
            put_tag(&mut new, number, WireType::Varint);
            put_varint(&mut new, value);
            let at = insert_position(&all, number);
            edits.push((at..at, new));
        }
        Some(_) if value == 0 => remove_all(&mut edits, &found),
        Some((first, rest)) => {
            let current = read_varint(msg, first.value.start)?.0;
            if current != value {
                let mut new = Vec::new();
                put_varint(&mut new, value);
                edits.push((first.value.clone(), new));
            }
            remove_all(&mut edits, rest);
        }
    }
    Ok(rebuild(msg, &edits))
}

/// Sets the length-delimited field `number` of `msg` (bytes or a string) to `bytes`, as
/// [`set_varint`] sets a varint: the first occurrence is replaced in place (its tag kept),
/// repeats are dropped, empty `bytes` remove the field, an absent field is inserted. Nothing in the
/// product writes a bytes field yet (the plan sets varints and pin entries; a contact's identity
/// bytes stay the phone's, DEVIATION L4), so only the tests call it.
#[cfg(test)]
pub(crate) fn set_bytes(msg: &[u8], number: u32, bytes: &[u8]) -> Result<Vec<u8>, WireError> {
    let all = fields(msg)?;
    let found = occurrences(&all, number, WireType::Len)?;
    let mut edits = Vec::new();
    match found.split_first() {
        None if bytes.is_empty() => {}
        None => {
            let mut new = Vec::new();
            put_len_field(&mut new, number, bytes);
            let at = insert_position(&all, number);
            edits.push((at..at, new));
        }
        Some(_) if bytes.is_empty() => remove_all(&mut edits, &found),
        Some((first, rest)) => {
            if msg[first.value.clone()] != *bytes {
                let mut new = Vec::new();
                put_varint(&mut new, bytes.len() as u64);
                new.extend_from_slice(bytes);
                edits.push((first.tag_end..first.span.end, new));
            }
            remove_all(&mut edits, rest);
        }
    }
    Ok(rebuild(msg, &edits))
}

/// Edits the sub-message `number` of `msg` (`StorageRecord`'s oneof: [`num::RECORD_CONTACT`],
/// [`num::RECORD_GROUP_V2`], [`num::RECORD_ACCOUNT`]): `edit` gets the sub-message's bytes
/// and returns the new ones (built with this module's functions), the length prefix is
/// rewritten and every other byte of `msg` stays. `Missing` when the sub-message is not there,
/// `Repeated` when it is there twice (a decoder would merge them; we do not guess).
pub(crate) fn edit_message(
    msg: &[u8],
    number: u32,
    edit: impl FnOnce(&[u8]) -> Result<Vec<u8>, WireError>,
) -> Result<Vec<u8>, WireError> {
    let all = fields(msg)?;
    let found = occurrences(&all, number, WireType::Len)?;
    let field = match found.as_slice() {
        [] => return Err(WireError::Missing),
        [one] => *one,
        _ => return Err(WireError::Repeated),
    };
    let edited = edit(&msg[field.value.clone()])?;
    let mut new = Vec::new();
    put_varint(&mut new, edited.len() as u64);
    new.extend_from_slice(&edited);
    Ok(rebuild(msg, &[(field.tag_end..field.span.end, new)]))
}

/// Keeps the occurrences of the repeated message field `number` for which `keep` (given the
/// occurrence's bytes) says yes, byte for byte, and drops the others; every other field stays.
pub(crate) fn retain_repeated(
    msg: &[u8],
    number: u32,
    mut keep: impl FnMut(&[u8]) -> bool,
) -> Result<Vec<u8>, WireError> {
    let all = fields(msg)?;
    let found = occurrences(&all, number, WireType::Len)?;
    let mut edits = Vec::new();
    for field in found {
        if !keep(&msg[field.value.clone()]) {
            edits.push((field.span.clone(), Vec::new()));
        }
    }
    Ok(rebuild(msg, &edits))
}

/// Adds one occurrence of the repeated message field `number` with the content `payload`,
/// after the existing ones (the new entry of a list comes last).
pub(crate) fn push_message(msg: &[u8], number: u32, payload: &[u8]) -> Result<Vec<u8>, WireError> {
    let all = fields(msg)?;
    occurrences(&all, number, WireType::Len)?;
    let mut new = Vec::new();
    put_len_field(&mut new, number, payload);
    let at = insert_position(&all, number);
    Ok(rebuild(msg, &[(at..at, new)]))
}

/// A new pin entry for a 1:1 chat, in the form of his Android phone (BQ5): `PinnedConversation
/// { contact { serviceIdBinary = the 16-byte ACI } }`, no string `serviceId`. Note to Self is
/// our own ACI.
pub(crate) fn contact_pin_entry(aci: Aci) -> Vec<u8> {
    // PinnedConversation.contact = 1; Contact.serviceIdBinary = 3.
    let mut contact = Vec::new();
    put_len_field(&mut contact, 3, &aci.service_id_binary());
    let mut entry = Vec::new();
    put_len_field(&mut entry, 1, &contact);
    entry
}

/// A new pin entry for a group: `PinnedConversation { groupMasterKey }`.
pub(crate) fn group_pin_entry(master_key: &[u8; 32]) -> Vec<u8> {
    // PinnedConversation.groupMasterKey = 4.
    let mut entry = Vec::new();
    put_len_field(&mut entry, 4, master_key);
    entry
}

#[cfg(test)]
mod tests {
    use libsignal_core::Aci;
    use prost::Message as _;
    use prost::encoding::{WireType as ProstWire, encode_key, encode_varint};
    use uuid::Uuid;

    use super::*;
    use crate::proto::{
        self, account_record::PinnedConversation, account_record::pinned_conversation,
    };

    // The inputs are built with prost's own encoding functions (not this module's), so the
    // expected bytes of a test come from another implementation.

    fn tag(number: u32, wire: ProstWire) -> Vec<u8> {
        let mut out = Vec::new();
        encode_key(number, wire, &mut out);
        out
    }

    fn vint(number: u32, value: u64) -> Vec<u8> {
        let mut out = tag(number, ProstWire::Varint);
        encode_varint(value, &mut out);
        out
    }

    fn lenf(number: u32, payload: &[u8]) -> Vec<u8> {
        let mut out = tag(number, ProstWire::LengthDelimited);
        encode_varint(payload.len() as u64, &mut out);
        out.extend_from_slice(payload);
        out
    }

    fn cat(parts: &[&[u8]]) -> Vec<u8> {
        parts.concat()
    }

    fn aci(id: u128) -> Aci {
        Aci::from(Uuid::from_u128(id))
    }

    /// A contact record in ascending field order with: an unknown varint 999 at the end, an
    /// unknown length-delimited 998, and the nickname (22) holding an unknown field of its own.
    struct Contact {
        aci: Vec<u8>,
        muted: Vec<u8>,
        nickname: Vec<u8>,
        unknown_a: Vec<u8>,
        unknown_b: Vec<u8>,
    }

    fn contact() -> Contact {
        Contact {
            aci: lenf(1, b"synthetic-aci-string"),
            muted: vint(13, 5),
            // Name { given = "Given", (nested unknown 99 varint), family = "Family", (nested
            // unknown 98 length-delimited) }
            nickname: lenf(
                22,
                &cat(&[
                    &lenf(1, b"Given"),
                    &vint(99, 7),
                    &lenf(2, b"Family"),
                    &lenf(98, &[0xff, 0x00, 0xfe]),
                ]),
            ),
            unknown_a: lenf(998, &[0xde, 0xad]),
            unknown_b: vint(999, 3),
        }
    }

    impl Contact {
        fn bytes(&self) -> Vec<u8> {
            cat(&[
                &self.aci,
                &self.muted,
                &self.nickname,
                &self.unknown_a,
                &self.unknown_b,
            ])
        }
    }

    #[test]
    fn an_edit_keeps_every_other_byte() {
        let c = contact();
        let input = c.bytes();

        // Replace in place: field 13 (5 -> a six-byte varint), nothing else moves.
        let big = 1_700_000_000_000u64;
        let out = set_varint(&input, num::CONTACT_MUTED_UNTIL, big).unwrap();
        let expected = cat(&[
            &c.aci,
            &vint(13, big),
            &c.nickname,
            &c.unknown_a,
            &c.unknown_b,
        ]);
        assert_eq!(out, expected);
        assert_ne!(out, input);

        // Insert: field 11 was absent and goes after field 1, before 13 (an encoder's order).
        let out = set_varint(&input, num::CONTACT_ARCHIVED, 1).unwrap();
        let expected = cat(&[
            &c.aci,
            &vint(11, 1),
            &c.muted,
            &c.nickname,
            &c.unknown_a,
            &c.unknown_b,
        ]);
        assert_eq!(out, expected);
        // A decoder agrees on the value, and the unknown bytes are still there to the byte.
        let decoded = proto::ContactRecord::decode(out.as_slice()).unwrap();
        assert!(decoded.archived);
        assert_eq!(decoded.muted_until_timestamp, 5);
        assert_eq!(decoded.nickname.unwrap().given, "Given");
        assert!(out.windows(c.nickname.len()).any(|w| w == c.nickname));

        // Through the StorageRecord's oneof: the contact is edited, the length prefix follows,
        // an unknown field of the StorageRecord itself stays.
        let record = cat(&[&lenf(1, &input), &vint(50, 1)]);
        let out = edit_message(&record, num::RECORD_CONTACT, |inner| {
            set_varint(inner, num::CONTACT_ARCHIVED, 1)
        })
        .unwrap();
        let edited_contact = cat(&[
            &c.aci,
            &vint(11, 1),
            &c.muted,
            &c.nickname,
            &c.unknown_a,
            &c.unknown_b,
        ]);
        assert_eq!(out, cat(&[&lenf(1, &edited_contact), &vint(50, 1)]));
        let decoded = proto::StorageRecord::decode(out.as_slice()).unwrap();
        assert!(matches!(
            decoded.record,
            Some(proto::storage_record::Record::Contact(ref c)) if c.archived
        ));

        // The input was not touched, and the same edit twice gives the same bytes.
        assert_eq!(input, c.bytes());
        assert_eq!(
            set_varint(&input, num::CONTACT_ARCHIVED, 1).unwrap(),
            set_varint(&input, num::CONTACT_ARCHIVED, 1).unwrap()
        );

        // set_bytes: replaces the first occurrence's content, everything else stays.
        let out = set_bytes(&input, 1, b"other-aci").unwrap();
        let expected = cat(&[
            &lenf(1, b"other-aci"),
            &c.muted,
            &c.nickname,
            &c.unknown_a,
            &c.unknown_b,
        ]);
        assert_eq!(out, expected);
    }

    #[test]
    fn an_equal_value_is_left_as_written() {
        // `archived = 1` written as an over-long varint stays as it is when we set 1.
        let input = cat(&[&lenf(1, b"x"), &[0x58, 0x81, 0x00], &vint(13, 5)]);
        assert_eq!(set_varint(&input, 11, 1).unwrap(), input);
        // The same for a string whose length is written over-long: 0a 81 00 'a'.
        let input = vec![0x0a, 0x81, 0x00, b'a'];
        assert_eq!(set_bytes(&input, 1, b"a").unwrap(), input);
        // A different content is written canonically.
        assert_eq!(set_bytes(&input, 1, b"b").unwrap(), lenf(1, b"b"));
    }

    #[test]
    fn repeats_are_dropped() {
        let unknown = vint(999, 3);
        // archived: 1, an unknown field between, then two more occurrences (0 and 1).
        let input = cat(&[
            &lenf(1, b"x"),
            &vint(11, 1),
            &unknown,
            &vint(11, 0),
            &vint(11, 1),
        ]);
        let out = set_varint(&input, 11, 1).unwrap();
        assert_eq!(out, cat(&[&lenf(1, b"x"), &vint(11, 1), &unknown]));

        // A changed value: the first occurrence takes it, the rest go.
        let input = cat(&[&vint(13, 1), &unknown, &vint(13, 2)]);
        let out = set_varint(&input, 13, 9).unwrap();
        assert_eq!(out, cat(&[&vint(13, 9), &unknown]));
        let decoded = proto::ContactRecord::decode(out.as_slice()).unwrap();
        assert_eq!(decoded.muted_until_timestamp, 9);

        // Bytes: two occurrences of the aci string, one left, with the new content.
        let input = cat(&[&lenf(1, b"a"), &unknown, &lenf(1, b"b")]);
        let out = set_bytes(&input, 1, b"c").unwrap();
        assert_eq!(out, cat(&[&lenf(1, b"c"), &unknown]));
    }

    #[test]
    fn zero_removes_the_field() {
        let unknown = vint(999, 3);
        let input = cat(&[
            &lenf(1, b"x"),
            &vint(11, 1),
            &vint(12, 1),
            &vint(13, 5),
            &unknown,
        ]);
        // archived true -> false: the field is gone (no `58 00`), the others stay.
        let out = set_varint(&input, 11, 0).unwrap();
        assert_eq!(
            out,
            cat(&[&lenf(1, b"x"), &vint(12, 1), &vint(13, 5), &unknown])
        );
        assert!(!out.windows(2).any(|w| w == [0x58, 0x00]));
        let decoded = proto::ContactRecord::decode(out.as_slice()).unwrap();
        assert!(!decoded.archived);
        assert!(decoded.marked_unread);

        // unmuted: field 13 gone.
        let out = set_varint(&input, 13, 0).unwrap();
        assert_eq!(
            out,
            cat(&[&lenf(1, b"x"), &vint(11, 1), &vint(12, 1), &unknown])
        );

        // Every occurrence of a repeated field goes with 0.
        let input = cat(&[&vint(11, 1), &unknown, &vint(11, 1)]);
        assert_eq!(set_varint(&input, 11, 0).unwrap(), unknown);

        // Zero on an absent field changes nothing.
        let input = cat(&[&lenf(1, b"x"), &unknown]);
        assert_eq!(set_varint(&input, 11, 0).unwrap(), input);

        // Empty bytes remove a bytes field.
        let input = cat(&[&lenf(1, b"x"), &vint(11, 1)]);
        assert_eq!(set_bytes(&input, 1, b"").unwrap(), vint(11, 1));
    }

    /// Four pins: a contact in the old form with a nested unknown field, a group, release
    /// notes and a number-only contact with an unknown field of the entry; around them other
    /// fields of the account record and unknown ones.
    struct Account {
        profile: Vec<u8>,
        archived: Vec<u8>,
        contact_pin: Vec<u8>,
        group_pin: Vec<u8>,
        release_pin: Vec<u8>,
        number_pin: Vec<u8>,
        badge: Vec<u8>,
        unknown: Vec<u8>,
    }

    fn account() -> Account {
        let contact = cat(&[
            &lenf(1, b"synthetic-service-id"),
            &lenf(3, &[0x11; 16]),
            &vint(77, 1),
        ]);
        Account {
            profile: lenf(1, &[0xaa; 32]),
            archived: vint(5, 1),
            contact_pin: lenf(14, &lenf(1, &contact)),
            group_pin: lenf(14, &lenf(4, &[0x22; 32])),
            release_pin: lenf(14, &lenf(5, &[])),
            number_pin: lenf(14, &cat(&[&lenf(1, &lenf(2, b"+1000")), &vint(42, 1)])),
            badge: vint(53, 2),
            unknown: lenf(999, &[1, 2, 3]),
        }
    }

    impl Account {
        fn bytes(&self) -> Vec<u8> {
            cat(&[
                &self.profile,
                &self.archived,
                &self.contact_pin,
                &self.group_pin,
                &self.release_pin,
                &self.number_pin,
                &self.badge,
                &self.unknown,
            ])
        }
    }

    #[test]
    fn a_pin_entry_is_kept_byte_for_byte() {
        let a = account();
        let input = a.bytes();

        // Keep everything: the same bytes.
        assert_eq!(
            retain_repeated(&input, num::ACCOUNT_PINNED, |_| true).unwrap(),
            input
        );

        // Drop the group: the other three entries (with their unknown fields) and every
        // other field are copied as they were.
        let out = retain_repeated(&input, num::ACCOUNT_PINNED, |entry| {
            let pin = PinnedConversation::decode(entry).unwrap();
            !matches!(
                pin.identifier,
                Some(pinned_conversation::Identifier::GroupMasterKey(_))
            )
        })
        .unwrap();
        let expected = cat(&[
            &a.profile,
            &a.archived,
            &a.contact_pin,
            &a.release_pin,
            &a.number_pin,
            &a.badge,
            &a.unknown,
        ]);
        assert_eq!(out, expected);

        // A new entry goes after the last pin, before the fields that follow them.
        let new = group_pin_entry(&[0x33; 32]);
        let out = push_message(&input, num::ACCOUNT_PINNED, &new).unwrap();
        let expected = cat(&[
            &a.profile,
            &a.archived,
            &a.contact_pin,
            &a.group_pin,
            &a.release_pin,
            &a.number_pin,
            &lenf(14, &new),
            &a.badge,
            &a.unknown,
        ]);
        assert_eq!(out, expected);
        let decoded = proto::AccountRecord::decode(out.as_slice()).unwrap();
        assert_eq!(decoded.pinned_conversations.len(), 5);
        assert!(decoded.note_to_self_archived);

        // Drop all, then push into a record with no pins: the entry lands after field 5.
        let none = retain_repeated(&input, num::ACCOUNT_PINNED, |_| false).unwrap();
        assert_eq!(none, cat(&[&a.profile, &a.archived, &a.badge, &a.unknown]));
        let out = push_message(&none, num::ACCOUNT_PINNED, &new).unwrap();
        assert_eq!(
            out,
            cat(&[
                &a.profile,
                &a.archived,
                &lenf(14, &new),
                &a.badge,
                &a.unknown
            ])
        );

        // The same inside a StorageRecord, as the plan will do it: drop the release notes pin.
        let record = lenf(num::RECORD_ACCOUNT, &input);
        let out = edit_message(&record, num::RECORD_ACCOUNT, |inner| {
            retain_repeated(inner, num::ACCOUNT_PINNED, |entry| {
                let pin = PinnedConversation::decode(entry).unwrap();
                !matches!(
                    pin.identifier,
                    Some(pinned_conversation::Identifier::ReleaseNotes(_))
                )
            })
        })
        .unwrap();
        let expected = cat(&[
            &a.profile,
            &a.archived,
            &a.contact_pin,
            &a.group_pin,
            &a.number_pin,
            &a.badge,
            &a.unknown,
        ]);
        assert_eq!(out, lenf(num::RECORD_ACCOUNT, &expected));
    }

    #[test]
    fn a_new_pin_entry_is_service_id_binary_only() {
        let who = aci(0xada);
        let entry = contact_pin_entry(who);
        // 0a 12 (contact, 18 bytes) 1a 10 (serviceIdBinary, 16 bytes) + the ACI.
        let mut expected = vec![0x0a, 0x12, 0x1a, 0x10];
        expected.extend_from_slice(&who.service_id_binary());
        assert_eq!(entry, expected);
        assert_eq!(who.service_id_binary().len(), 16);

        // Walked: one field (the contact), holding one field (the binary id); no string
        // `serviceId` (1) and no `e164` (2).
        let outer = fields(&entry).unwrap();
        assert_eq!(outer.len(), 1);
        assert_eq!(outer[0].number, 1);
        let inner_bytes = &entry[outer[0].value.clone()];
        let inner = fields(inner_bytes).unwrap();
        assert_eq!(inner.len(), 1);
        assert_eq!(inner[0].number, 3);

        // A decoder reads it as the phone does.
        let pin = PinnedConversation::decode(entry.as_slice()).unwrap();
        let Some(pinned_conversation::Identifier::Contact(contact)) = pin.identifier else {
            panic!("not a contact pin");
        };
        assert_eq!(contact.service_id, "");
        assert_eq!(contact.e164, "");
        assert_eq!(contact.service_id_binary, who.service_id_binary());

        // A group: groupMasterKey (4) and nothing else.
        let group = group_pin_entry(&[0x44; 32]);
        let mut expected = vec![0x22, 0x20];
        expected.extend_from_slice(&[0x44; 32]);
        assert_eq!(group, expected);
        let pin = PinnedConversation::decode(group.as_slice()).unwrap();
        assert!(matches!(
            pin.identifier,
            Some(pinned_conversation::Identifier::GroupMasterKey(ref key)) if key == &[0x44; 32]
        ));

        // Pushed into a record, the entry is the encoded `PinnedConversation` as a field 14.
        let out = push_message(&[], num::ACCOUNT_PINNED, &entry).unwrap();
        assert_eq!(out, lenf(14, &entry));
    }

    /// Every way a message can be malformed, as (name, bytes, the error).
    fn malformed() -> Vec<(&'static str, Vec<u8>, WireError)> {
        let mut long = vec![0x58];
        long.extend_from_slice(&[0x80; 10]);
        long.push(0x00);
        let mut too_big = vec![0x58];
        too_big.extend_from_slice(&[0xff; 9]);
        too_big.push(0x02);
        let mut huge_len = vec![0x0a];
        huge_len.extend_from_slice(&[0xff; 9]);
        huge_len.push(0x01);
        vec![
            ("truncated tag", vec![0x80], WireError::Truncated),
            (
                "no value after a varint tag",
                vec![0x58],
                WireError::Truncated,
            ),
            ("truncated varint", vec![0x58, 0x80], WireError::Truncated),
            ("eleven-byte varint", long, WireError::BadVarint),
            ("varint over 64 bits", too_big, WireError::BadVarint),
            (
                "length past the end",
                vec![0x0a, 0x05, 1, 2],
                WireError::BadLength,
            ),
            // One byte more than the message has, the case an off-by-one check lets through.
            (
                "length one past the end",
                vec![0x0a, 0x03, 1, 2],
                WireError::BadLength,
            ),
            ("length missing", vec![0x0a], WireError::Truncated),
            ("length of u64::MAX", huge_len, WireError::BadLength),
            (
                "truncated fixed64",
                vec![0x09, 1, 2, 3],
                WireError::Truncated,
            ),
            (
                "fixed64 one byte short",
                vec![0x09, 1, 2, 3, 4, 5, 6, 7],
                WireError::Truncated,
            ),
            ("truncated fixed32", vec![0x0d, 1, 2], WireError::Truncated),
            (
                "fixed32 one byte short",
                vec![0x0d, 1, 2, 3],
                WireError::Truncated,
            ),
            ("start group", vec![0x0b], WireError::UnsupportedType),
            ("end group", vec![0x0c], WireError::UnsupportedType),
            (
                "a whole group",
                vec![0x0b, 0x0c],
                WireError::UnsupportedType,
            ),
            ("wire type 6", vec![0x0e, 0x00], WireError::UnsupportedType),
            ("wire type 7", vec![0x0f, 0x00], WireError::UnsupportedType),
            ("field number 0", vec![0x00, 0x01], WireError::BadTag),
            (
                "tag over 32 bits",
                vec![0xff, 0xff, 0xff, 0xff, 0x1f, 0x00],
                WireError::BadTag,
            ),
        ]
    }

    #[test]
    fn malformed_input_is_refused() {
        for (name, bytes, error) in malformed() {
            assert_eq!(fields(&bytes), Err(error), "{name}");
            // Every edit refuses it, and refuses it whatever it asks for.
            assert_eq!(set_varint(&bytes, 11, 1), Err(error), "{name}");
            assert_eq!(set_varint(&bytes, 11, 0), Err(error), "{name}");
            assert_eq!(set_bytes(&bytes, 1, b"a"), Err(error), "{name}");
            assert_eq!(set_bytes(&bytes, 1, b""), Err(error), "{name}");
            assert_eq!(retain_repeated(&bytes, 14, |_| true), Err(error), "{name}");
            assert_eq!(push_message(&bytes, 14, &[1]), Err(error), "{name}");
            let mut called = false;
            assert_eq!(
                edit_message(&bytes, 1, |inner| {
                    called = true;
                    Ok(inner.to_vec())
                }),
                Err(error),
                "{name}"
            );
            assert!(!called, "{name}: the edit ran on a malformed message");

            // A sound field to edit FIRST and the broken bytes after it: still refused, so no
            // edit is ever half done.
            let tail = cat(&[&vint(11, 0), &lenf(1, b"a"), &bytes]);
            assert_eq!(set_varint(&tail, 11, 1), Err(error), "{name} after a field");
            assert_eq!(
                set_bytes(&tail, 1, b"b"),
                Err(error),
                "{name} after a field"
            );
            assert_eq!(
                retain_repeated(&tail, 1, |_| false),
                Err(error),
                "{name} after a field"
            );
        }
        // Not malformed, but empty: no fields, and the edits work.
        assert_eq!(fields(&[]), Ok(vec![]));
        assert_eq!(set_varint(&[], 11, 1).unwrap(), vint(11, 1));
        assert_eq!(set_varint(&[], 11, 0).unwrap(), Vec::<u8>::new());

        // A sub-message that is malformed inside: the outer message walks, the edit of the
        // inner one is refused.
        let record = lenf(1, &[0x0b]);
        assert!(fields(&record).is_ok());
        assert_eq!(
            edit_message(&record, num::RECORD_CONTACT, |inner| set_varint(
                inner, 11, 1
            )),
            Err(WireError::UnsupportedType)
        );
        // A refused edit is an error of the crate's, log-safe.
        let error: Error = WireError::BadLength.into();
        assert!(matches!(error, Error::Decode(_)));
    }

    #[test]
    fn an_edit_that_does_not_fit_is_refused() {
        // A field with another wire type than the edit needs.
        let input = lenf(11, b"x");
        assert_eq!(set_varint(&input, 11, 1), Err(WireError::WrongType));
        let input = vint(1, 1);
        assert_eq!(set_bytes(&input, 1, b"x"), Err(WireError::WrongType));
        assert_eq!(
            retain_repeated(&input, 1, |_| true),
            Err(WireError::WrongType)
        );
        assert_eq!(push_message(&input, 1, b"x"), Err(WireError::WrongType));
        assert_eq!(
            edit_message(&input, 1, |m| Ok(m.to_vec())),
            Err(WireError::WrongType)
        );
        // The sub-message to edit is missing, or there are two of it.
        let input = vint(50, 1);
        assert_eq!(
            edit_message(&input, 1, |m| Ok(m.to_vec())),
            Err(WireError::Missing)
        );
        let input = cat(&[&lenf(1, b"a"), &lenf(1, b"b")]);
        assert_eq!(
            edit_message(&input, 1, |m| Ok(m.to_vec())),
            Err(WireError::Repeated)
        );
        // The edit's own error comes back, and nothing is returned.
        let input = lenf(1, b"a");
        assert_eq!(
            edit_message(&input, 1, |_| Err(WireError::Missing)),
            Err(WireError::Missing)
        );
    }

    #[test]
    fn fields_are_located_without_decoding() {
        let c = contact();
        let input = c.bytes();
        let all = fields(&input).unwrap();
        assert_eq!(
            all.iter().map(|f| f.number).collect::<Vec<_>>(),
            [1, 13, 22, 998, 999]
        );
        // The spans tile the message, a value lies inside its span.
        let mut at = 0;
        for f in &all {
            assert_eq!(f.span.start, at);
            assert!(f.span.start < f.tag_end && f.tag_end <= f.value.start);
            assert!(f.value.end == f.span.end);
            at = f.span.end;
        }
        assert_eq!(at, input.len());
        assert_eq!(all[0].wire_type, WireType::Len);
        assert_eq!(&input[all[0].value.clone()], b"synthetic-aci-string");
        assert_eq!(all[1].wire_type, WireType::Varint);
        assert_eq!(&input[all[1].value.clone()], [5]);
        // Fixed-size values.
        let mut fixed = tag(7, ProstWire::SixtyFourBit);
        fixed.extend_from_slice(&[1; 8]);
        fixed.extend_from_slice(&tag(8, ProstWire::ThirtyTwoBit));
        fixed.extend_from_slice(&[2; 4]);
        let all = fields(&fixed).unwrap();
        assert_eq!(all[0].wire_type, WireType::Fixed64);
        assert_eq!(&fixed[all[0].value.clone()], [1; 8]);
        assert_eq!(all[1].wire_type, WireType::Fixed32);
        assert_eq!(&fixed[all[1].value.clone()], [2; 4]);
        // An edit leaves them alone too.
        assert_eq!(
            set_varint(&fixed, 11, 1).unwrap(),
            cat(&[&fixed, &vint(11, 1)])
        );
    }

    /// Each number in [`num`] is the number the generated types use (an encoded message with
    /// only that field set starts with that tag).
    #[test]
    fn the_field_numbers_match_the_proto() {
        fn first_number(bytes: Vec<u8>) -> u32 {
            let all = fields(&bytes).unwrap();
            assert_eq!(all.len(), 1);
            all[0].number
        }
        let contact = |f: fn(&mut proto::ContactRecord)| {
            let mut r = proto::ContactRecord::default();
            f(&mut r);
            first_number(r.encode_to_vec())
        };
        assert_eq!(
            contact(|r| r.identity_key = vec![1]),
            num::CONTACT_IDENTITY_KEY
        );
        assert_eq!(
            contact(|r| r.set_identity_state(proto::contact_record::IdentityState::Verified)),
            num::CONTACT_IDENTITY_STATE
        );
        assert_eq!(contact(|r| r.archived = true), num::CONTACT_ARCHIVED);
        assert_eq!(
            contact(|r| r.marked_unread = true),
            num::CONTACT_MARKED_UNREAD
        );
        assert_eq!(
            contact(|r| r.muted_until_timestamp = 1),
            num::CONTACT_MUTED_UNTIL
        );
        let group = |f: fn(&mut proto::GroupV2Record)| {
            let mut r = proto::GroupV2Record::default();
            f(&mut r);
            first_number(r.encode_to_vec())
        };
        assert_eq!(group(|r| r.archived = true), num::GROUP_ARCHIVED);
        assert_eq!(group(|r| r.marked_unread = true), num::GROUP_MARKED_UNREAD);
        assert_eq!(
            group(|r| r.muted_until_timestamp = 1),
            num::GROUP_MUTED_UNTIL
        );
        let account = |f: fn(&mut proto::AccountRecord)| {
            let mut r = proto::AccountRecord::default();
            f(&mut r);
            first_number(r.encode_to_vec())
        };
        assert_eq!(
            account(|r| r.note_to_self_archived = true),
            num::ACCOUNT_NOTE_TO_SELF_ARCHIVED
        );
        assert_eq!(
            account(|r| r.note_to_self_marked_unread = true),
            num::ACCOUNT_NOTE_TO_SELF_MARKED_UNREAD
        );
        assert_eq!(
            account(|r| r.pinned_conversations.push(PinnedConversation::default())),
            num::ACCOUNT_PINNED
        );
        let record = |r: proto::storage_record::Record| {
            first_number(proto::StorageRecord { record: Some(r) }.encode_to_vec())
        };
        assert_eq!(
            record(proto::storage_record::Record::Contact(Default::default())),
            num::RECORD_CONTACT
        );
        assert_eq!(
            record(proto::storage_record::Record::GroupV2(Default::default())),
            num::RECORD_GROUP_V2
        );
        assert_eq!(
            record(proto::storage_record::Record::Account(Default::default())),
            num::RECORD_ACCOUNT
        );
    }
}
