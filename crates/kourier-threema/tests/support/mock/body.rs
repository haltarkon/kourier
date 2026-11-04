//! CSP E2E message BODIES as a phone reflects them (TH5 A2; `TP/csp.struct.yml`, `TP/csp-e2e.proto`,
//! `TP/common.proto`). A reflected `IncomingMessage.body` / `OutgoingMessage.body` is "the unpadded
//! `csp.e2e.container.padded-data`" (`TP/md-d2d.proto`): the inner message struct, with NO PKCS#7 padding,
//! and for a group message the `group-member-container` around it. Written from the spec text and
//! NEVER from Kourier's parser (rule 9: the fake is the second implementation); from libthreema only the
//! generated protobuf types (`csp_e2e::{EditMessage, DeleteMessage, Reaction}`).
//!
//! Marks: `[U: real phone]` where the pinned spec does not fix the bytes a real phone writes, `[I]` for a
//! reading of ours. Nothing here prints a body: every builder returns bytes, and the types that hold
//! them print lengths.
//!
//! Message ids are 8 bytes in WIRE order ([`MsgId`]): the `message-id` of the spec is `u64-le`, so the
//! protobuf's `fixed64` is `u64::from_le_bytes(id)` and the text form (a quote, the contract's `local`)
//! is the 8 bytes as 16 lowercase hex digits, the form of the crate's `msg_id::format`.

use libthreema::protobuf::csp_e2e;
use prost::Message as _;

/// A message id in wire order (the 8 bytes `msg_id::format` / `MessageId::to_bytes` give).
pub type MsgId = [u8; 8];

/// `message-id` / `fixed64` of the spec: the 8 wire bytes as a little-endian `u64`.
pub fn id_u64(id: MsgId) -> u64 {
    u64::from_le_bytes(id)
}

/// The text form of an id: the 8 wire bytes as 16 lowercase hex digits (`quotes.ts` `u64ToHexLe`).
pub fn id_hex(id: MsgId) -> String {
    id.iter().map(|b| format!("{b:02x}")).collect()
}

/// The CSP E2E message types (`TP/common.proto` `CspE2eMessageType`, the container's `type` u8).
pub mod ty {
    /// `_INVALID_TYPE`: a body with this type is invalid.
    pub const INVALID: u8 = 0x00;
    pub const TEXT: u8 = 0x01;
    pub const DEPRECATED_IMAGE: u8 = 0x02;
    pub const LOCATION: u8 = 0x10;
    pub const DEPRECATED_VIDEO: u8 = 0x13;
    pub const DEPRECATED_AUDIO: u8 = 0x14;
    pub const POLL_SETUP: u8 = 0x15;
    pub const POLL_VOTE: u8 = 0x16;
    pub const FILE: u8 = 0x17;
    pub const CONTACT_SET_PROFILE_PICTURE: u8 = 0x18;
    pub const CONTACT_DELETE_PROFILE_PICTURE: u8 = 0x19;
    pub const CONTACT_REQUEST_PROFILE_PICTURE: u8 = 0x1a;
    pub const GROUP_TEXT: u8 = 0x41;
    pub const GROUP_LOCATION: u8 = 0x42;
    pub const GROUP_IMAGE: u8 = 0x43;
    pub const GROUP_VIDEO: u8 = 0x44;
    pub const GROUP_AUDIO: u8 = 0x45;
    pub const GROUP_FILE: u8 = 0x46;
    pub const GROUP_SETUP: u8 = 0x4a;
    pub const GROUP_NAME: u8 = 0x4b;
    pub const GROUP_LEAVE: u8 = 0x4c;
    pub const GROUP_CALL_START: u8 = 0x4f;
    pub const GROUP_SET_PROFILE_PICTURE: u8 = 0x50;
    pub const GROUP_SYNC_REQUEST: u8 = 0x51;
    pub const GROUP_POLL_SETUP: u8 = 0x52;
    pub const GROUP_POLL_VOTE: u8 = 0x53;
    pub const GROUP_DELETE_PROFILE_PICTURE: u8 = 0x54;
    pub const CALL_OFFER: u8 = 0x60;
    pub const CALL_ANSWER: u8 = 0x61;
    pub const CALL_ICE_CANDIDATE: u8 = 0x62;
    pub const CALL_HANGUP: u8 = 0x63;
    pub const CALL_RINGING: u8 = 0x64;
    pub const DELIVERY_RECEIPT: u8 = 0x80;
    pub const GROUP_DELIVERY_RECEIPT: u8 = 0x81;
    pub const REACTION: u8 = 0x82;
    pub const GROUP_REACTION: u8 = 0x83;
    pub const TYPING_INDICATOR: u8 = 0x90;
    pub const EDIT_MESSAGE: u8 = 0x91;
    pub const DELETE_MESSAGE: u8 = 0x92;
    pub const GROUP_EDIT_MESSAGE: u8 = 0x93;
    pub const GROUP_DELETE_MESSAGE: u8 = 0x94;
    pub const FORWARD_SECURITY_ENVELOPE: u8 = 0xa0;
    pub const EMPTY: u8 = 0xfc;
    pub const WORK_SYNC_DELTA: u8 = 0xfd;
    pub const WEB_SESSION_RESUME: u8 = 0xfe;
    /// A number the pinned `CspE2eMessageType` does not have (a newer protocol's message).
    pub const UNKNOWN: u8 = 0x7e;
}

/// Every named type with the enum's own name (`CspE2eMessageType::from_str_name`): the test holds the
/// numbers against the generated enum, and every enum value against this list.
pub const TYPE_NAMES: &[(&str, u8)] = &[
    ("_INVALID_TYPE", ty::INVALID),
    ("TEXT", ty::TEXT),
    ("DEPRECATED_IMAGE", ty::DEPRECATED_IMAGE),
    ("LOCATION", ty::LOCATION),
    ("DEPRECATED_VIDEO", ty::DEPRECATED_VIDEO),
    ("DEPRECATED_AUDIO", ty::DEPRECATED_AUDIO),
    ("POLL_SETUP", ty::POLL_SETUP),
    ("POLL_VOTE", ty::POLL_VOTE),
    ("FILE", ty::FILE),
    (
        "CONTACT_SET_PROFILE_PICTURE",
        ty::CONTACT_SET_PROFILE_PICTURE,
    ),
    (
        "CONTACT_DELETE_PROFILE_PICTURE",
        ty::CONTACT_DELETE_PROFILE_PICTURE,
    ),
    (
        "CONTACT_REQUEST_PROFILE_PICTURE",
        ty::CONTACT_REQUEST_PROFILE_PICTURE,
    ),
    ("GROUP_TEXT", ty::GROUP_TEXT),
    ("GROUP_LOCATION", ty::GROUP_LOCATION),
    ("GROUP_IMAGE", ty::GROUP_IMAGE),
    ("GROUP_VIDEO", ty::GROUP_VIDEO),
    ("GROUP_AUDIO", ty::GROUP_AUDIO),
    ("GROUP_FILE", ty::GROUP_FILE),
    ("GROUP_SETUP", ty::GROUP_SETUP),
    ("GROUP_NAME", ty::GROUP_NAME),
    ("GROUP_LEAVE", ty::GROUP_LEAVE),
    ("GROUP_CALL_START", ty::GROUP_CALL_START),
    ("GROUP_SET_PROFILE_PICTURE", ty::GROUP_SET_PROFILE_PICTURE),
    ("GROUP_SYNC_REQUEST", ty::GROUP_SYNC_REQUEST),
    ("GROUP_POLL_SETUP", ty::GROUP_POLL_SETUP),
    ("GROUP_POLL_VOTE", ty::GROUP_POLL_VOTE),
    (
        "GROUP_DELETE_PROFILE_PICTURE",
        ty::GROUP_DELETE_PROFILE_PICTURE,
    ),
    ("CALL_OFFER", ty::CALL_OFFER),
    ("CALL_ANSWER", ty::CALL_ANSWER),
    ("CALL_ICE_CANDIDATE", ty::CALL_ICE_CANDIDATE),
    ("CALL_HANGUP", ty::CALL_HANGUP),
    ("CALL_RINGING", ty::CALL_RINGING),
    ("DELIVERY_RECEIPT", ty::DELIVERY_RECEIPT),
    ("GROUP_DELIVERY_RECEIPT", ty::GROUP_DELIVERY_RECEIPT),
    ("REACTION", ty::REACTION),
    ("GROUP_REACTION", ty::GROUP_REACTION),
    ("TYPING_INDICATOR", ty::TYPING_INDICATOR),
    ("EDIT_MESSAGE", ty::EDIT_MESSAGE),
    ("DELETE_MESSAGE", ty::DELETE_MESSAGE),
    ("GROUP_EDIT_MESSAGE", ty::GROUP_EDIT_MESSAGE),
    ("GROUP_DELETE_MESSAGE", ty::GROUP_DELETE_MESSAGE),
    ("FORWARD_SECURITY_ENVELOPE", ty::FORWARD_SECURITY_ENVELOPE),
    ("EMPTY", ty::EMPTY),
    ("WORK_SYNC_DELTA", ty::WORK_SYNC_DELTA),
    ("WEB_SESSION_RESUME", ty::WEB_SESSION_RESUME),
];

/// The group form of a 1:1 message type: the type a phone reflects for the same message sent into a
/// group (wrapped by [`group_member_container`]). `None` for a type that has no group form (typing,
/// calls, the control messages).
pub fn group_type(one_to_one: u8) -> Option<u8> {
    Some(match one_to_one {
        ty::TEXT => ty::GROUP_TEXT,
        ty::LOCATION => ty::GROUP_LOCATION,
        ty::DEPRECATED_IMAGE => ty::GROUP_IMAGE,
        ty::DEPRECATED_VIDEO => ty::GROUP_VIDEO,
        ty::DEPRECATED_AUDIO => ty::GROUP_AUDIO,
        ty::FILE => ty::GROUP_FILE,
        ty::POLL_SETUP => ty::GROUP_POLL_SETUP,
        ty::POLL_VOTE => ty::GROUP_POLL_VOTE,
        ty::DELIVERY_RECEIPT => ty::GROUP_DELIVERY_RECEIPT,
        ty::REACTION => ty::GROUP_REACTION,
        ty::EDIT_MESSAGE => ty::GROUP_EDIT_MESSAGE,
        ty::DELETE_MESSAGE => ty::GROUP_DELETE_MESSAGE,
        _ => return None,
    })
}

/// `e2e.text` (`TP/csp.struct.yml:2937`): "UTF-8 encoded text", nothing else.
pub fn text(text: &str) -> Vec<u8> {
    text.as_bytes().to_vec()
}

/// A quote in a text: `> quote #<16 hex>` + two line feeds + the comment. [I] The pinned spec does not
/// describe it; this is the form every Threema app writes (Desktop `quotes.ts:9` and `:62`,
/// `serializeQuoteText`: `` `> quote #${u64ToHexLe(id)}\n\n${comment}` ``, the hex being the 8 wire
/// bytes in order). [U: real phone] that Android and iOS write the same form.
pub fn quote(quoted: MsgId, comment: &str) -> String {
    format!("> quote #{}\n\n{comment}", id_hex(quoted))
}

/// A mention in a text: `@[` + the 8-character Threema ID + `]` (Desktop `mentions.ts:4`,
/// `/@\[(?<identity>[A-Z0-9*]{1}[A-Z0-9]{7}|@{8})\]/`). [I] not in the pinned spec.
pub fn mention(identity: &str) -> String {
    assert!(
        super::is_threema_id(identity),
        "a mention needs a Threema ID"
    );
    format!("@[{identity}]")
}

/// The mention of everybody: `@[@@@@@@@@]` (Desktop `EVERYONE_IDENTITY_STRING`). [I]
pub const MENTION_ALL: &str = "@[@@@@@@@@]";

/// `e2e.group-member-container` (`TP/csp.struct.yml:2886`): `creator-identity` (8 bytes, the Threema ID's
/// ASCII), `group-id` (`u64-le`), then the inner message struct. The wrapper of most messages a group
/// member sends.
#[track_caller]
pub fn group_member_container(creator: &str, group_id: u64, inner: &[u8]) -> Vec<u8> {
    assert!(creator.len() == 8, "a creator identity has 8 bytes");
    let mut out = Vec::with_capacity(16 + inner.len());
    out.extend_from_slice(creator.as_bytes());
    out.extend_from_slice(&group_id.to_le_bytes());
    out.extend_from_slice(inner);
    out
}

/// `e2e.group-creator-container` (`TP/csp.struct.yml:2870`): `group-id` (`u64-le`), then the inner
/// message struct. The wrapper of the special group messages the creator sends (group setup, name,
/// picture) and members send back (sync request); the creator is the sender.
pub fn group_creator_container(group_id: u64, inner: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + inner.len());
    out.extend_from_slice(&group_id.to_le_bytes());
    out.extend_from_slice(inner);
    out
}

/// Where a location is, beyond its coordinates (`TP/csp.struct.yml:3163` `location`).
#[derive(Clone, PartialEq, Eq)]
pub enum Place {
    /// Coordinates only.
    Nothing,
    /// `<address>` on the second line.
    Address(String),
    /// `<name>` on the second line and `<address>` on the third (a point of interest).
    Named { name: String, address: String },
}

impl std::fmt::Debug for Place {
    /// The variant only, never the address.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Nothing => "Place::Nothing",
            Self::Address(_) => "Place::Address",
            Self::Named { .. } => "Place::Named",
        })
    }
}

/// `e2e.location`: `<lat>,<lon>[,<accuracy>]` then, by `place`, `\n<address>` or `\n<name>\n<address>`.
/// The values are strings as the spec writes them ("a WGS-84 string", "a floating point number formatted
/// as a string"); a line feed inside an address is escaped as a literal backslash and `n`
/// ("each line feed must be escaped (literal `\n`)").
pub fn location(lat: &str, lon: &str, accuracy: Option<&str>, place: &Place) -> Vec<u8> {
    let escape = |s: &str| s.replace('\n', "\\n");
    let mut out = format!("{lat},{lon}");
    if let Some(accuracy) = accuracy {
        out.push(',');
        out.push_str(accuracy);
    }
    match place {
        Place::Nothing => {}
        Place::Address(address) => {
            out.push('\n');
            out.push_str(&escape(address));
        }
        Place::Named { name, address } => {
            out.push('\n');
            out.push_str(&escape(name));
            out.push('\n');
            out.push_str(&escape(address));
        }
    }
    out.into_bytes()
}

/// What a file message's JSON says about the media (`'x'`, `TP/csp.struct.yml:3640-3660`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FileMeta {
    /// Width (`'w'`) in px, images and video.
    pub width: Option<u32>,
    /// Height (`'h'`) in px, images and video.
    pub height: Option<u32>,
    /// Animated (`'a'`), images.
    pub animated: Option<bool>,
    /// Duration (`'d'`) in seconds as the JSON number's text (`"12.5"`), audio and video.
    pub duration: Option<String>,
}

/// The fields of `e2e.file`'s JSON (`TP/csp.struct.yml:3607-3640`).
#[derive(Clone, PartialEq, Eq)]
pub struct FileSpec {
    /// Rendering type (`'j'`): 0 file, 1 media, 2 sticker.
    pub rendering: u8,
    /// Encryption key (`'k'`), lowercase hex.
    pub key_hex: String,
    /// File blob ID (`'b'`), lowercase hex.
    pub blob_hex: String,
    /// File media type (`'m'`).
    pub media_type: String,
    /// File name (`'n'`).
    pub name: Option<String>,
    /// File size (`'s'`) in bytes.
    pub size: u64,
    /// Thumbnail blob ID (`'t'`), lowercase hex.
    pub thumbnail_hex: Option<String>,
    /// Thumbnail media type (`'p'`).
    pub thumbnail_type: Option<String>,
    /// Caption (`'d'`).
    pub caption: Option<String>,
    /// Correlation ID (`'c'`): a 32-byte ASCII string.
    pub correlation: Option<String>,
    /// Metadata (`'x'`).
    pub meta: Option<FileMeta>,
}

impl std::fmt::Debug for FileSpec {
    /// Never the key, the blob ids, the name or the caption.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileSpec")
            .field("rendering", &self.rendering)
            .field("size", &self.size)
            .finish()
    }
}

impl FileSpec {
    /// A synthetic picture: media rendering, `image/jpeg`, a 64-hex key and a 32-hex blob id of
    /// repeated digits (no real blob), 640x480.
    pub fn picture(name: &str, caption: Option<&str>) -> Self {
        Self {
            rendering: 1,
            key_hex: "ab".repeat(32),
            blob_hex: "cd".repeat(16),
            media_type: "image/jpeg".to_owned(),
            name: Some(name.to_owned()),
            size: 123_456,
            thumbnail_hex: Some("ef".repeat(16)),
            thumbnail_type: Some("image/jpeg".to_owned()),
            caption: caption.map(str::to_owned),
            correlation: None,
            meta: Some(FileMeta {
                width: Some(640),
                height: Some(480),
                animated: Some(false),
                duration: None,
            }),
        }
    }
}

fn json_string(out: &mut String, value: &str) {
    out.push('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if u32::from(c) < 0x20 => out.push_str(&format!("\\u{:04x}", u32::from(c))),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// `e2e.file`: "UTF-8, JSON-encoded object" with the spec's one-letter keys, in the fixed order
/// `j i k b t p m n s d c x`. `'i'` (deprecated) is 1 when the rendering type is 1 or 2, else 0
/// (`TP/csp.struct.yml:3616`). Absent options are left out, never `null`.
pub fn file(spec: &FileSpec) -> Vec<u8> {
    let mut o = String::from("{");
    o.push_str(&format!("\"j\":{}", spec.rendering));
    o.push_str(&format!(
        ",\"i\":{}",
        u8::from(matches!(spec.rendering, 1 | 2))
    ));
    for (key, value) in [("k", Some(&spec.key_hex)), ("b", Some(&spec.blob_hex))] {
        if let Some(value) = value {
            o.push_str(&format!(",\"{key}\":"));
            json_string(&mut o, value);
        }
    }
    for (key, value) in [
        ("t", spec.thumbnail_hex.as_ref()),
        ("p", spec.thumbnail_type.as_ref()),
        ("m", Some(&spec.media_type)),
        ("n", spec.name.as_ref()),
    ] {
        if let Some(value) = value {
            o.push_str(&format!(",\"{key}\":"));
            json_string(&mut o, value);
        }
    }
    o.push_str(&format!(",\"s\":{}", spec.size));
    for (key, value) in [
        ("d", spec.caption.as_ref()),
        ("c", spec.correlation.as_ref()),
    ] {
        if let Some(value) = value {
            o.push_str(&format!(",\"{key}\":"));
            json_string(&mut o, value);
        }
    }
    if let Some(meta) = &spec.meta {
        o.push_str(",\"x\":{");
        let mut fields = Vec::new();
        if let Some(w) = meta.width {
            fields.push(format!("\"w\":{w}"));
        }
        if let Some(h) = meta.height {
            fields.push(format!("\"h\":{h}"));
        }
        if let Some(a) = meta.animated {
            fields.push(format!("\"a\":{a}"));
        }
        if let Some(d) = &meta.duration {
            fields.push(format!("\"d\":{d}"));
        }
        o.push_str(&fields.join(","));
        o.push('}');
    }
    o.push('}');
    o.into_bytes()
}

/// `e2e.delivery-receipt` (`TP/csp.struct.yml:4213`): `status` (u8: 1 received, 2 read, 3 and 4 the
/// deprecated thumbs up and down) then one or more `message-id`s (`u64-le[]`).
pub fn delivery_receipt(status: u8, ids: &[MsgId]) -> Vec<u8> {
    let mut out = vec![status];
    for id in ids {
        out.extend_from_slice(id);
    }
    out
}

/// `e2e.typing-indicator` (`TP/csp.struct.yml:4388`): `is-typing`, one byte, "`1` in case the contact is
/// currently typing or `0` in case the contact stopped typing. Other values are invalid" (a test may
/// pass another one on purpose).
pub fn typing(is_typing: u8) -> Vec<u8> {
    vec![is_typing]
}

/// `csp_e2e.EditMessage` (`TP/csp-e2e.proto:209`): the message to edit and its new text.
pub fn edit_message(target: MsgId, text: &str) -> Vec<u8> {
    csp_e2e::EditMessage {
        message_id: id_u64(target),
        text: text.to_owned(),
    }
    .encode_to_vec()
}

/// `csp_e2e.DeleteMessage` (`TP/csp-e2e.proto:352`).
pub fn delete_message(target: MsgId) -> Vec<u8> {
    csp_e2e::DeleteMessage {
        message_id: id_u64(target),
    }
    .encode_to_vec()
}

/// `csp_e2e.Reaction` with `apply` (`TP/csp-e2e.proto:615`): "a single emoji reaction", the bytes of
/// its UTF-8 (at most 64 bytes, not empty: a test may pass invalid ones).
pub fn reaction_apply(target: MsgId, emoji: &[u8]) -> Vec<u8> {
    csp_e2e::Reaction {
        message_id: id_u64(target),
        action: Some(csp_e2e::reaction::Action::Apply(emoji.to_vec())),
    }
    .encode_to_vec()
}

/// `csp_e2e.Reaction` with `withdraw`.
pub fn reaction_withdraw(target: MsgId, emoji: &[u8]) -> Vec<u8> {
    csp_e2e::Reaction {
        message_id: id_u64(target),
        action: Some(csp_e2e::reaction::Action::Withdraw(emoji.to_vec())),
    }
    .encode_to_vec()
}

/// A `csp_e2e.Reaction` with NO action (the oneof unset): an invalid reaction.
pub fn reaction_without_action(target: MsgId) -> Vec<u8> {
    csp_e2e::Reaction {
        message_id: id_u64(target),
        action: None,
    }
    .encode_to_vec()
}

/// `len` opaque bytes (`seed`, `seed + 1`, ...) for a kind whose body Kourier keeps raw or does not
/// read at all (a poll, a legacy picture, a call signal, a type from a newer protocol): the contents
/// are not the spec's, only the type and the length are. [U: real phone] the real bodies.
pub fn filler(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| seed.wrapping_add(u8::try_from(i % 251).expect("below 256")))
        .collect()
}
