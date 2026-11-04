//! THE CLASSIFICATION TABLE (TH5 A6; brief §2.5 as amended by `th5-decisions.md`): what a reflected
//! envelope IS, before anything is built from it. ONE module, two tables:
//!
//! - [`ENVELOPE_KINDS`]: every content kind of the `d2d.Envelope` oneof (the pinned `md-d2d.proto`);
//! - [`CSP_TYPES`]: every value of `CspE2eMessageType` (the pinned `common.proto`).
//!
//! A message envelope (`IncomingMessage`, `OutgoingMessage`) is classified by its CSP type; every other
//! kind by its own row. The class says what the map does with it:
//!
//! - [`Class::Mapped`]: events (TH5a: text and the contact sync (A7a); TH5b B1: the location and the file; B2: the
//!   edit, the delete and the reaction; B3: the delivery receipts, 1:1 and group, and the sent and read updates;
//!   B4: the settings, profile and distribution list syncs, a list's with no event);
//! - [`Class::Notice`]: events and `Body::System` rows (a group sync);
//! - [`Class::Ephemeral`]: typing (decision M5: the Reflected FLAG decides the ack, the kind decides the event).
//!   An envelope WITH the flag never reaches this table: `reflect::open_ephemeral` states its `Typing` (TH5b B3,
//!   R12); a typing indicator reflected WITHOUT the flag is acked, its nonce written, and states nothing;
//! - [`Class::Deferred`]: the plaintext is KEPT in `deferred` under [`MAP_VERSION`] and acked; a later build
//!   maps it (decision C3: a modifier and a kind whose step is later are never a bubble);
//! - [`Class::Unsupported`]: a `Body::Unsupported` row with the raw body (the viewmodel's words for the hint);
//! - [`Class::Dropped`]: acked, nothing kept, a counted line by the rule's name (decisions M2, M6).
//!
//! The tests hold both tables against the pinned protos at run time: a value of the generated enum, a
//! variant of the oneof or a line of the `.proto` file that has no row (or a row with no value, or a number
//! twice) fails, so a libthreema update that adds a type stops here until it is classified.
//!
//! Pure: no I/O, no clock, no logging.

use libthreema::protobuf::d2d;
use libthreema::protobuf::d2d::envelope::Content;

/// The mapping version of this build (`deferred.map_version`): TH5a's was 1; TH5b's first chunk (B1: the
/// location and the file) is 2 (decision R14), and it stays 2 for the whole of TH5b (B2's edit, delete and
/// reaction leave the deferred list in the same build: no file of a build between B1 and B2 exists). RAISE it
/// whenever a kind leaves the deferred list or a rule changes what a deferred row becomes AFTER a build that
/// deferred it was in use (the start maps the rows of a lower version again, A8).
pub(crate) const MAP_VERSION: u32 = 2;

/// What a kind of reflected envelope is (module doc).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Class {
    Mapped,
    Notice,
    Ephemeral,
    /// Kept whole under [`MAP_VERSION`]; the name is the step that maps it (log-safe, static).
    Deferred(&'static str),
    /// A `Body::Unsupported { kind_hint }` row; the hint is static and log-safe.
    Unsupported(&'static str),
    /// Acked and dropped; the rule's name is the counted line's.
    Dropped(&'static str),
}

/// The rule of a content-less envelope (`content: None`, a oneof this build does not know).
const KIND_ENVELOPE: &str = "envelope";

/// The deferred kind of anything addressed to a distribution list (lists are stored, not stated).
pub(crate) const KIND_LIST: &str = "list";

/// The deferred kind of a CSP type number the pinned enum lacks (decision M6: never a bubble).
pub(crate) const KIND_UNKNOWN_TYPE: &str = "unknown-type";

/// The rule of a call's ICE candidate and ringing (round B, M-2 = S2): signalling no later build can use.
pub(crate) const RULE_CALL_SIGNAL: &str = "call-signal";

/// The rules whose drop (or, for a map rule, skip) is the protocol's NORMAL path (a V0.2+ device reflects the
/// CSP group control beside the `GroupSync` that carries it, a contact's picture is in the `ContactSync`,
/// `EMPTY` is nothing, a call's ICE candidates and ringing are of no later use, our phone's "received" receipt
/// and the Sent update of a notes group say nothing new): counted, but not a WARN.
pub(crate) const QUIET_RULES: &[&str] = &[
    "group-control",
    "contact-control",
    "empty",
    RULE_CALL_SIGNAL,
    RULE_OWN_DELIVERED,
    RULE_SENT_NOTES_GROUP,
];

/// The map's rule (TH5b B3, R9) of our own phone's delivery receipt of status 1 ("received") to a peer: our
/// device got the peer's message, nothing for the core to show. Counted QUIET: it is the protocol's normal
/// path, one per received message.
pub(crate) const RULE_OWN_DELIVERED: &str = "receipt-own-delivered";

/// The map's rule (TH5b FIX-P, P-M1) of the `OutgoingMessageUpdate.Sent` of a message in a NOTES group (the
/// user created it, nobody else is in it): the phone reflects it ("Reflecting with `receivers` empty is a
/// legitimate case that occurs when sending a message in a notes group", `csp.struct.yml`), but the message
/// was stated `Sent` at its arrival, so the update has nothing left to say. A SKIP counted QUIET: it is the
/// protocol's normal path, one per message of the chat.
pub(crate) const RULE_SENT_NOTES_GROUP: &str = "sent-notes-group";

/// THE SYNC KINDS (round B, M-2 = S2 (a)): deltas of a stored BLOB (the settings, the user's profile, the
/// distribution lists) that TH5a deferred. Losing one leaves the stored data wrong for good, and they are
/// few, so `deferred`'s row and byte bounds never evict them and its 45-day age bound never expires them
/// (`store::add_deferred`, `store::expire_deferred`). TH5b B4 (R13) MAPS all three at arrival, so no row of
/// this build is deferred under them (N-1 closed by construction: the table test holds it); a version-1 row of
/// TH5a keeps the kind until the start's re-map applies it, so the names stay.
pub(crate) const EXEMPT_KINDS: [&str; 3] = ["settings", "profile", "list-sync"];

/// THE LOW-VALUE KINDS (round B, M-2 = S2 (c)): delivery receipts, the read / sent updates and the call
/// kinds that are still deferred. Over a bound, `deferred` evicts these first (oldest first), then the
/// rest oldest first, so a full table loses a receipt before a file's key, an edit or a delete. TH5b B3 maps
/// the receipts and the updates: no row of this build is deferred as `receipt`, `read` or `sent` any more,
/// but a version-1 row of TH5a may still carry the kind until the start's re-map takes it (a page left by a
/// failed zone, a store that failed), so the names stay (the classify test holds them as TH5a's kinds).
pub(crate) const LOW_VALUE_KINDS: [&str; 5] = ["receipt", "read", "sent", "call", "group-call"];

/// The rule of a group control message from a device that speaks V0.1 (or says nothing): the spec's
/// "the group is going to desync" warning.
pub(crate) const RULE_GROUP_DESYNC: &str = "group-control-desync";

/// Whether `rule` is counted without a WARN ([`QUIET_RULES`]).
pub(crate) fn is_quiet(rule: &str) -> bool {
    QUIET_RULES.contains(&rule)
}

/// What an envelope kind's row says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Row {
    /// A message: the CSP type decides ([`CSP_TYPES`]).
    ByType,
    /// The kind's own class.
    Is(Class),
}

/// Every content of `d2d.Envelope`, by the proto's field name (the test holds the names against
/// `md-d2d.proto`'s `oneof content`).
pub(crate) const ENVELOPE_KINDS: &[(&str, Row)] = &[
    ("outgoing_message", Row::ByType),
    ("incoming_message", Row::ByType),
    // TH5b B3 (R10, R11): the sent and the read updates.
    ("outgoing_message_update", Row::Is(Class::Mapped)),
    ("incoming_message_update", Row::Is(Class::Mapped)),
    // TH5b B4 (R13): the user's profile (`AccountReady`), the lists (stored, stated by a later lists step)
    // and the settings (`AccountSettings`, the blocked contacts) are applied to their BLOBs at arrival.
    ("user_profile_sync", Row::Is(Class::Mapped)),
    ("contact_sync", Row::Is(Class::Mapped)),
    ("group_sync", Row::Is(Class::Notice)),
    ("distribution_list_sync", Row::Is(Class::Mapped)),
    ("settings_sync", Row::Is(Class::Mapped)),
    // Work only (consumer IDs, his TT2).
    ("mdm_parameter_sync", Row::Is(Class::Dropped("mdm"))),
];

/// The proto field name of an envelope content (exhaustive: a oneof variant a libthreema update adds is a
/// compile error here, and its name is a failing line of the table test).
pub(crate) fn kind_name(content: &Content) -> &'static str {
    match content {
        Content::OutgoingMessage(_) => "outgoing_message",
        Content::OutgoingMessageUpdate(_) => "outgoing_message_update",
        Content::IncomingMessage(_) => "incoming_message",
        Content::IncomingMessageUpdate(_) => "incoming_message_update",
        Content::UserProfileSync(_) => "user_profile_sync",
        Content::ContactSync(_) => "contact_sync",
        Content::GroupSync(_) => "group_sync",
        Content::DistributionListSync(_) => "distribution_list_sync",
        Content::SettingsSync(_) => "settings_sync",
        Content::MdmParameterSync(_) => "mdm_parameter_sync",
    }
}

/// Every `CspE2eMessageType`: (the proto's name, its number, its class). The test holds the names and the
/// numbers against `common.proto` and the generated enum, both ways.
pub(crate) const CSP_TYPES: &[(&str, u8, Class)] = &[
    ("_INVALID_TYPE", 0x00, Class::Dropped("invalid-type")),
    ("EMPTY", 0xfc, Class::Dropped("empty")),
    // 1:1 messages
    ("TEXT", 0x01, Class::Mapped),
    ("DEPRECATED_IMAGE", 0x02, Class::Unsupported("legacy-media")),
    ("LOCATION", 0x10, Class::Mapped),
    ("DEPRECATED_AUDIO", 0x14, Class::Unsupported("legacy-media")),
    ("DEPRECATED_VIDEO", 0x13, Class::Unsupported("legacy-media")),
    ("FILE", 0x17, Class::Mapped),
    ("POLL_SETUP", 0x15, Class::Unsupported("poll")),
    ("POLL_VOTE", 0x16, Class::Deferred("poll-vote")),
    // A call's offer, answer and hangup wait for the call lines (TH5b); its ICE candidates (dozens per
    // call) and its ringing are signalling no later build can use: DROPPED, never deferred (round B, M-2 =
    // S2: they would fill the bounds of `deferred` with rows nothing ever maps).
    ("CALL_OFFER", 0x60, Class::Deferred("call")),
    ("CALL_ANSWER", 0x61, Class::Deferred("call")),
    ("CALL_ICE_CANDIDATE", 0x62, Class::Dropped(RULE_CALL_SIGNAL)),
    ("CALL_HANGUP", 0x63, Class::Deferred("call")),
    ("CALL_RINGING", 0x64, Class::Dropped(RULE_CALL_SIGNAL)),
    // 1:1 message updates
    ("DELIVERY_RECEIPT", 0x80, Class::Mapped),
    ("TYPING_INDICATOR", 0x90, Class::Ephemeral),
    ("REACTION", 0x82, Class::Mapped),
    ("EDIT_MESSAGE", 0x91, Class::Mapped),
    ("DELETE_MESSAGE", 0x92, Class::Mapped),
    // 1:1 contact control: covered by the `ContactSync` (Desktop does the same).
    (
        "CONTACT_SET_PROFILE_PICTURE",
        0x18,
        Class::Dropped("contact-control"),
    ),
    (
        "CONTACT_DELETE_PROFILE_PICTURE",
        0x19,
        Class::Dropped("contact-control"),
    ),
    (
        "CONTACT_REQUEST_PROFILE_PICTURE",
        0x1a,
        Class::Dropped("contact-control"),
    ),
    // Group control: V0.2+ devices carry it in the `GroupSync`; from a V0.1 device the map says
    // `group-control-desync` (the spec's warning). Never applied.
    ("GROUP_SETUP", 0x4a, Class::Dropped("group-control")),
    ("GROUP_NAME", 0x4b, Class::Dropped("group-control")),
    ("GROUP_LEAVE", 0x4c, Class::Dropped("group-control")),
    (
        "GROUP_SET_PROFILE_PICTURE",
        0x50,
        Class::Dropped("group-control"),
    ),
    (
        "GROUP_DELETE_PROFILE_PICTURE",
        0x54,
        Class::Dropped("group-control"),
    ),
    ("GROUP_SYNC_REQUEST", 0x51, Class::Dropped("group-control")),
    ("GROUP_CALL_START", 0x4f, Class::Deferred("group-call")),
    // Group messages
    ("GROUP_TEXT", 0x41, Class::Mapped),
    ("GROUP_LOCATION", 0x42, Class::Mapped),
    ("GROUP_IMAGE", 0x43, Class::Unsupported("legacy-media")),
    ("GROUP_AUDIO", 0x45, Class::Unsupported("legacy-media")),
    ("GROUP_VIDEO", 0x44, Class::Unsupported("legacy-media")),
    ("GROUP_FILE", 0x46, Class::Mapped),
    ("GROUP_POLL_SETUP", 0x52, Class::Unsupported("poll")),
    ("GROUP_POLL_VOTE", 0x53, Class::Deferred("poll-vote")),
    // Group message updates (a group's delivery receipt carries only legacy reactions: R9 maps those, the map
    // drops the rest `group-receipt`)
    ("GROUP_DELIVERY_RECEIPT", 0x81, Class::Mapped),
    ("GROUP_EDIT_MESSAGE", 0x93, Class::Mapped),
    ("GROUP_DELETE_MESSAGE", 0x94, Class::Mapped),
    ("GROUP_REACTION", 0x83, Class::Mapped),
    // Never reflected (Desktop warns for the three): dropped.
    (
        "FORWARD_SECURITY_ENVELOPE",
        0xa0,
        Class::Dropped("forward-security"),
    ),
    ("WORK_SYNC_DELTA", 0xfd, Class::Dropped("work-sync-delta")),
    (
        "WEB_SESSION_RESUME",
        0xfe,
        Class::Dropped("web-session-resume"),
    ),
];

/// The class of a CSP type NUMBER: its row, or (decision M6) [`Class::Deferred`] `unknown-type` for a
/// number the pinned enum lacks.
pub(crate) fn of_type(number: i32) -> Class {
    u8::try_from(number)
        .ok()
        .and_then(|n| CSP_TYPES.iter().find(|(_, v, _)| *v == n))
        .map_or(Class::Deferred(KIND_UNKNOWN_TYPE), |(_, _, class)| *class)
}

/// What a reflected envelope is: `None` content is deferred as `envelope` (a oneof this build does not
/// know); an `OutgoingMessage` to a distribution list is deferred as `list` whatever its type; any other
/// message is classified by its CSP type, every other kind by its row.
pub(crate) fn classify(content: Option<&Content>) -> Class {
    let Some(content) = content else {
        return Class::Deferred(KIND_ENVELOPE);
    };
    match content {
        Content::OutgoingMessage(m)
            if matches!(
                m.conversation.as_ref().and_then(|c| c.id.as_ref()),
                Some(d2d::conversation_id::Id::DistributionList(_))
            ) =>
        {
            Class::Deferred(KIND_LIST)
        }
        Content::OutgoingMessage(m) => of_type(m.r#type),
        Content::IncomingMessage(m) => of_type(m.r#type),
        other => {
            let name = kind_name(other);
            match ENVELOPE_KINDS.iter().find(|(n, _)| *n == name) {
                Some((_, Row::Is(class))) => *class,
                // Unreachable while the table test holds (a message kind is matched above); the
                // honest answer for a table without a row is to keep the envelope.
                Some((_, Row::ByType)) | None => Class::Deferred(KIND_ENVELOPE),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::path::Path;

    use libthreema::protobuf::common::CspE2eMessageType;

    use super::*;

    fn protocols() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../third_party/libthreema/threema-protocols/src")
    }

    fn read(file: &str) -> String {
        std::fs::read_to_string(protocols().join(file)).expect("read the pinned proto")
    }

    /// The `(NAME, value)` lines of `enum CspE2eMessageType { ... }` in `common.proto`.
    fn proto_types() -> Vec<(String, u8)> {
        let text = read("common.proto");
        let start = text
            .find("enum CspE2eMessageType")
            .expect("the enum is in common.proto");
        let body = &text[start..];
        let body = &body[body.find('{').expect("open") + 1..body.find('}').expect("close")];
        body.lines()
            .filter_map(|line| {
                let line = line.split("//").next().unwrap_or("").trim();
                let (name, value) = line.strip_suffix(';')?.split_once('=')?;
                let value = value.trim().strip_prefix("0x")?;
                Some((
                    name.trim().to_owned(),
                    u8::from_str_radix(value, 16).expect("a hex value"),
                ))
            })
            .collect()
    }

    /// The field names of `message Envelope`'s `oneof content { ... }` in `md-d2d.proto`.
    fn proto_kinds() -> Vec<String> {
        let text = read("md-d2d.proto");
        let start = text.find("message Envelope {").expect("Envelope");
        let body = &text[start..];
        let oneof = &body[body.find("oneof content {").expect("oneof") + "oneof content {".len()..];
        let oneof = &oneof[..oneof.find('}').expect("end of the oneof")];
        oneof
            .lines()
            .filter_map(|line| {
                let line = line.split("//").next().unwrap_or("").trim();
                let (left, _) = line.strip_suffix(';')?.split_once('=')?;
                Some(left.split_whitespace().last()?.to_owned())
            })
            .collect()
    }

    /// One example of every oneof variant (the test needs the generated types' own names for them).
    fn samples() -> Vec<Content> {
        vec![
            Content::OutgoingMessage(Default::default()),
            Content::OutgoingMessageUpdate(Default::default()),
            Content::IncomingMessage(Default::default()),
            Content::IncomingMessageUpdate(Default::default()),
            Content::UserProfileSync(Default::default()),
            Content::ContactSync(Default::default()),
            Content::GroupSync(Default::default()),
            Content::DistributionListSync(Default::default()),
            Content::SettingsSync(Default::default()),
            Content::MdmParameterSync(Default::default()),
        ]
    }

    #[test]
    fn every_csp_type_of_the_pinned_proto_has_exactly_one_row() {
        let proto = proto_types();
        assert!(proto.len() > 40, "the proto parse found too few types");
        // The proto's lines against the table, both ways, name AND number.
        let table: BTreeSet<(String, u8)> = CSP_TYPES
            .iter()
            .map(|(name, number, _)| ((*name).to_owned(), *number))
            .collect();
        let pinned: BTreeSet<(String, u8)> = proto.into_iter().collect();
        assert_eq!(
            table.len(),
            CSP_TYPES.len(),
            "a name or a number is in the table twice"
        );
        let numbers: BTreeSet<u8> = CSP_TYPES.iter().map(|(_, n, _)| *n).collect();
        assert_eq!(numbers.len(), CSP_TYPES.len(), "a number has two rows");
        let missing: Vec<&(String, u8)> = pinned.difference(&table).collect();
        assert!(
            missing.is_empty(),
            "the pinned proto has types the table does not name: {missing:?}"
        );
        let extra: Vec<&(String, u8)> = table.difference(&pinned).collect();
        assert!(
            extra.is_empty(),
            "the table has rows the pinned proto does not have: {extra:?}"
        );
    }

    #[test]
    fn every_value_of_the_generated_enum_has_exactly_one_row() {
        let mut seen = 0usize;
        for number in 0..=i32::from(u8::MAX) {
            let Ok(ty) = CspE2eMessageType::try_from(number) else {
                // A number the enum lacks is the unknown-type row (decision M6), never a table row.
                assert_eq!(
                    of_type(number),
                    Class::Deferred(KIND_UNKNOWN_TYPE),
                    "a number the enum lacks is not deferred as unknown-type"
                );
                continue;
            };
            seen += 1;
            let rows: Vec<&(&str, u8, Class)> = CSP_TYPES
                .iter()
                .filter(|(_, n, _)| i32::from(*n) == number)
                .collect();
            assert_eq!(rows.len(), 1, "a value of the generated enum has no row");
            assert_eq!(
                rows[0].0,
                ty.as_str_name(),
                "the row's name is not the generated enum's"
            );
        }
        assert_eq!(seen, CSP_TYPES.len(), "the enum and the table differ");
    }

    #[test]
    fn every_content_of_the_envelope_oneof_has_exactly_one_row() {
        let proto: BTreeSet<String> = proto_kinds().into_iter().collect();
        assert_eq!(
            proto.len(),
            10,
            "the oneof parse found {} kinds",
            proto.len()
        );
        let table: BTreeSet<&str> = ENVELOPE_KINDS.iter().map(|(n, _)| *n).collect();
        assert_eq!(table.len(), ENVELOPE_KINDS.len(), "a kind has two rows");
        let table_owned: BTreeSet<String> = table.iter().map(|n| (*n).to_owned()).collect();
        assert_eq!(table_owned, proto, "the table and the pinned oneof differ");
        // The generated enum's variants (one sample each) reach their rows by name.
        let named: BTreeSet<&str> = samples().iter().map(kind_name).collect();
        assert_eq!(named, table, "kind_name and the table differ");
    }

    #[test]
    fn the_classes_of_the_table_are_the_decisions() {
        let class = |name: &str| {
            CSP_TYPES
                .iter()
                .find(|(n, _, _)| *n == name)
                .map(|(_, _, c)| *c)
                .expect("a row")
        };
        // MAPPED: the two texts of TH5a, B1's location and file (R3, R4), B2's reaction, edit and delete
        // (R6-R8) and B3's delivery receipts (R9), in the table's order.
        let mapped: Vec<&str> = CSP_TYPES
            .iter()
            .filter(|(_, _, c)| *c == Class::Mapped)
            .map(|(n, _, _)| *n)
            .collect();
        assert_eq!(
            mapped,
            [
                "TEXT",
                "LOCATION",
                "FILE",
                "DELIVERY_RECEIPT",
                "REACTION",
                "EDIT_MESSAGE",
                "DELETE_MESSAGE",
                "GROUP_TEXT",
                "GROUP_LOCATION",
                "GROUP_FILE",
                "GROUP_DELIVERY_RECEIPT",
                "GROUP_EDIT_MESSAGE",
                "GROUP_DELETE_MESSAGE",
                "GROUP_REACTION"
            ]
        );
        assert_eq!(MAP_VERSION, 2, "B1 raised the mapping version to 2 (R14)");
        // What is still deferred at version 2 (R15): the votes, the calls, the lists, the unknown.
        assert_eq!(class("POLL_VOTE"), Class::Deferred("poll-vote"));
        assert_eq!(class("GROUP_POLL_VOTE"), Class::Deferred("poll-vote"));
        assert_eq!(class("CALL_OFFER"), Class::Deferred("call"));
        assert_eq!(class("GROUP_CALL_START"), Class::Deferred("group-call"));
        // Typing is ephemeral (M5, H4; R12 states it from the flagged envelope, never from this table).
        assert_eq!(class("TYPING_INDICATOR"), Class::Ephemeral);
        assert_eq!(class("_INVALID_TYPE"), Class::Dropped("invalid-type"));
        assert_eq!(class("POLL_SETUP"), Class::Unsupported("poll"));
        assert_eq!(class("GROUP_VIDEO"), Class::Unsupported("legacy-media"));
        // The kinds that are not messages.
        let kind = |name: &str| {
            ENVELOPE_KINDS
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, r)| *r)
                .expect("a row")
        };
        assert_eq!(kind("group_sync"), Row::Is(Class::Notice));
        assert_eq!(kind("outgoing_message_update"), Row::Is(Class::Mapped));
        assert_eq!(kind("incoming_message_update"), Row::Is(Class::Mapped));
        // B4 (R13): the settings, profile and list syncs.
        assert_eq!(kind("settings_sync"), Row::Is(Class::Mapped));
        assert_eq!(kind("user_profile_sync"), Row::Is(Class::Mapped));
        assert_eq!(kind("distribution_list_sync"), Row::Is(Class::Mapped));
        assert_eq!(kind("mdm_parameter_sync"), Row::Is(Class::Dropped("mdm")));
        assert_eq!(kind("incoming_message"), Row::ByType);
        // Every deferred kind is a name the deferred CHECK takes (1-32 of a-z, 0-9, -).
        for (_, _, c) in CSP_TYPES {
            let Class::Deferred(kind) = c else { continue };
            assert!(
                (1..=32).contains(&kind.len())
                    && kind
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'),
                "a deferred kind the CHECK refuses"
            );
        }
    }

    #[test]
    fn classify_follows_the_type_the_list_and_the_missing_content() {
        use d2d::conversation_id::Id;
        let outgoing = |id: Id, ty: CspE2eMessageType| {
            Content::OutgoingMessage(d2d::OutgoingMessage {
                conversation: Some(d2d::ConversationId { id: Some(id) }),
                r#type: ty as i32,
                ..Default::default()
            })
        };
        let incoming = |ty: i32| {
            Content::IncomingMessage(d2d::IncomingMessage {
                r#type: ty,
                ..Default::default()
            })
        };
        assert_eq!(classify(None), Class::Deferred("envelope"));
        assert_eq!(
            classify(Some(&incoming(CspE2eMessageType::Text as i32))),
            Class::Mapped
        );
        assert_eq!(
            classify(Some(&incoming(CspE2eMessageType::EditMessage as i32))),
            Class::Mapped
        );
        // A number the enum lacks: deferred, never a bubble (M6); 0x00 is dropped.
        assert_eq!(
            classify(Some(&incoming(0x7e))),
            Class::Deferred("unknown-type")
        );
        assert_eq!(
            classify(Some(&incoming(-1))),
            Class::Deferred("unknown-type")
        );
        assert_eq!(classify(Some(&incoming(0))), Class::Dropped("invalid-type"));
        // A message to a distribution list is deferred whatever its type.
        let to_list = outgoing(Id::DistributionList(5), CspE2eMessageType::Text);
        assert_eq!(classify(Some(&to_list)), Class::Deferred("list"));
        let to_contact = outgoing(Id::Contact("PEERAAAA".into()), CspE2eMessageType::Text);
        assert_eq!(classify(Some(&to_contact)), Class::Mapped);
        // The kinds that are not messages go by their rows.
        let sync = Content::GroupSync(Default::default());
        assert_eq!(classify(Some(&sync)), Class::Notice);
        let update = Content::OutgoingMessageUpdate(Default::default());
        assert_eq!(classify(Some(&update)), Class::Mapped);
    }

    /// TH5b B1 (R3, R4, R14): the location and the file are Mapped in both forms, incoming and outgoing,
    /// and the build's mapping version is 2; the votes and the calls are still deferred.
    #[test]
    fn the_location_and_the_file_are_mapped_in_both_forms_at_version_two() {
        use d2d::conversation_id::Id;
        for ty in [
            CspE2eMessageType::Location,
            CspE2eMessageType::GroupLocation,
            CspE2eMessageType::File,
            CspE2eMessageType::GroupFile,
        ] {
            let incoming = Content::IncomingMessage(d2d::IncomingMessage {
                r#type: ty as i32,
                ..Default::default()
            });
            assert_eq!(classify(Some(&incoming)), Class::Mapped, "{ty:?} incoming");
            let outgoing = Content::OutgoingMessage(d2d::OutgoingMessage {
                conversation: Some(d2d::ConversationId {
                    id: Some(Id::Contact("PEERAAAA".into())),
                }),
                r#type: ty as i32,
                ..Default::default()
            });
            assert_eq!(classify(Some(&outgoing)), Class::Mapped, "{ty:?} outgoing");
            // To a distribution list it is still `list`, whatever the type.
            let to_list = Content::OutgoingMessage(d2d::OutgoingMessage {
                conversation: Some(d2d::ConversationId {
                    id: Some(Id::DistributionList(5)),
                }),
                r#type: ty as i32,
                ..Default::default()
            });
            assert_eq!(classify(Some(&to_list)), Class::Deferred("list"));
        }
        assert_eq!(MAP_VERSION, 2);
        let vote = Content::IncomingMessage(d2d::IncomingMessage {
            r#type: CspE2eMessageType::PollVote as i32,
            ..Default::default()
        });
        assert_eq!(classify(Some(&vote)), Class::Deferred("poll-vote"));
    }

    /// TH5b B2 (R6-R8): the edit, the delete and the reaction are Mapped in both forms, incoming and outgoing;
    /// to a distribution list they are still `list`, and the build's mapping version stays 2 (one version for
    /// the whole of TH5b). The receipts: B3's test below.
    #[test]
    fn the_edit_the_delete_and_the_reaction_are_mapped_in_both_forms() {
        use d2d::conversation_id::Id;
        for ty in [
            CspE2eMessageType::EditMessage,
            CspE2eMessageType::GroupEditMessage,
            CspE2eMessageType::DeleteMessage,
            CspE2eMessageType::GroupDeleteMessage,
            CspE2eMessageType::Reaction,
            CspE2eMessageType::GroupReaction,
        ] {
            let incoming = Content::IncomingMessage(d2d::IncomingMessage {
                r#type: ty as i32,
                ..Default::default()
            });
            assert_eq!(classify(Some(&incoming)), Class::Mapped, "{ty:?} incoming");
            let outgoing = |id| {
                Content::OutgoingMessage(d2d::OutgoingMessage {
                    conversation: Some(d2d::ConversationId { id: Some(id) }),
                    r#type: ty as i32,
                    ..Default::default()
                })
            };
            assert_eq!(
                classify(Some(&outgoing(Id::Contact("PEERAAAA".into())))),
                Class::Mapped,
                "{ty:?} outgoing"
            );
            assert_eq!(
                classify(Some(&outgoing(Id::DistributionList(5)))),
                Class::Deferred("list"),
                "{ty:?} to a list"
            );
        }
        assert_eq!(MAP_VERSION, 2);
    }

    /// TH5b B3 (R9-R12, R15): both delivery receipts are Mapped, incoming and outgoing (to a list still
    /// `list`); the sent and the read updates are Mapped; typing stays Ephemeral (a typing reflected without the
    /// flag states nothing); the mapping version stays 2; what is still deferred at version 2 is R15's list.
    #[test]
    fn the_receipts_and_the_updates_are_mapped_and_typing_stays_ephemeral() {
        use d2d::conversation_id::Id;
        for ty in [
            CspE2eMessageType::DeliveryReceipt,
            CspE2eMessageType::GroupDeliveryReceipt,
        ] {
            let incoming = Content::IncomingMessage(d2d::IncomingMessage {
                r#type: ty as i32,
                ..Default::default()
            });
            assert_eq!(classify(Some(&incoming)), Class::Mapped, "{ty:?} incoming");
            let outgoing = |id| {
                Content::OutgoingMessage(d2d::OutgoingMessage {
                    conversation: Some(d2d::ConversationId { id: Some(id) }),
                    r#type: ty as i32,
                    ..Default::default()
                })
            };
            assert_eq!(
                classify(Some(&outgoing(Id::Contact("PEERAAAA".into())))),
                Class::Mapped,
                "{ty:?} outgoing"
            );
            assert_eq!(
                classify(Some(&outgoing(Id::DistributionList(5)))),
                Class::Deferred("list")
            );
        }
        for update in [
            Content::OutgoingMessageUpdate(Default::default()),
            Content::IncomingMessageUpdate(Default::default()),
        ] {
            assert_eq!(classify(Some(&update)), Class::Mapped);
        }
        let typing = Content::IncomingMessage(d2d::IncomingMessage {
            r#type: CspE2eMessageType::TypingIndicator as i32,
            ..Default::default()
        });
        assert_eq!(classify(Some(&typing)), Class::Ephemeral);
        assert_eq!(MAP_VERSION, 2);
        // R15: every kind the table still defers at version 2, CSP types and envelope kinds together.
        let still: BTreeSet<&str> = CSP_TYPES
            .iter()
            .map(|(_, _, c)| *c)
            .chain(ENVELOPE_KINDS.iter().filter_map(|(_, r)| match r {
                Row::Is(c) => Some(*c),
                Row::ByType => None,
            }))
            .filter_map(|c| match c {
                Class::Deferred(kind) => Some(kind),
                _ => None,
            })
            .collect();
        assert_eq!(
            still,
            BTreeSet::from(["call", "group-call", "poll-vote"]),
            "the table defers other kinds than R15's (B4 maps the syncs)"
        );
    }

    /// TH5b B4 (R13, N-1 closed by construction): the settings, profile and list syncs are Mapped, and NO row
    /// of either table defers an exempt kind any more, so the exempt rows of `deferred` can only be TH5a's
    /// version-1 rows, which the first start of this build re-maps (the names stay for them and for the
    /// eviction order).
    #[test]
    fn n_1_no_row_of_either_table_defers_a_sync_kind_and_the_three_syncs_are_mapped() {
        for sync in [
            Content::SettingsSync(Default::default()),
            Content::UserProfileSync(Default::default()),
            Content::DistributionListSync(Default::default()),
        ] {
            assert_eq!(classify(Some(&sync)), Class::Mapped);
        }
        let classes = CSP_TYPES
            .iter()
            .map(|(_, _, c)| *c)
            .chain(ENVELOPE_KINDS.iter().filter_map(|(_, r)| match r {
                Row::Is(c) => Some(*c),
                Row::ByType => None,
            }));
        for class in classes {
            if let Class::Deferred(kind) = class {
                assert!(
                    !EXEMPT_KINDS.contains(&kind),
                    "a row defers the exempt kind `{kind}`"
                );
            }
        }
        // The constants a row cannot name (a message to a list, an unknown type, no content) are no sync kind.
        for kind in [KIND_LIST, KIND_UNKNOWN_TYPE, KIND_ENVELOPE] {
            assert!(!EXEMPT_KINDS.contains(&kind), "`{kind}` is exempt");
        }
        assert_eq!(EXEMPT_KINDS, ["settings", "profile", "list-sync"]);
    }

    #[test]
    fn the_quiet_rules_are_rules_of_the_table() {
        let rules: BTreeSet<&str> = CSP_TYPES
            .iter()
            .filter_map(|(_, _, c)| match c {
                Class::Dropped(rule) => Some(*rule),
                _ => None,
            })
            .collect();
        // The map's two quiet rules (B3, R9: our own "received" receipt; FIX-P, P-M1: the Sent update of a
        // notes group) are no row's; every other is a row's.
        let maps = [RULE_OWN_DELIVERED, RULE_SENT_NOTES_GROUP];
        for quiet in QUIET_RULES.iter().filter(|r| !maps.contains(r)) {
            assert!(rules.contains(quiet), "a quiet rule no row drops by");
        }
        for quiet in maps {
            assert!(is_quiet(quiet), "`{quiet}` is warned");
            assert!(!rules.contains(quiet), "the map's rule is a row's");
        }
        assert_eq!(RULE_SENT_NOTES_GROUP, "sent-notes-group");
        assert!(!is_quiet(RULE_GROUP_DESYNC), "the desync warning is quiet");
        assert!(!is_quiet("invalid-type"));
    }

    /// Round B, M-2 = S2 (b): a call's ICE candidate and ringing are DROPPED (quietly); its offer, answer
    /// and hangup and a group call stay deferred for the call lines.
    #[test]
    fn call_signalling_is_dropped_and_the_call_lines_stay_deferred() {
        let class = |name: &str| {
            CSP_TYPES
                .iter()
                .find(|(n, _, _)| *n == name)
                .map(|(_, _, c)| *c)
                .expect("a row")
        };
        for name in ["CALL_ICE_CANDIDATE", "CALL_RINGING"] {
            assert_eq!(class(name), Class::Dropped(RULE_CALL_SIGNAL), "{name}");
        }
        assert!(is_quiet(RULE_CALL_SIGNAL), "call signalling is warned");
        for name in ["CALL_OFFER", "CALL_ANSWER", "CALL_HANGUP"] {
            assert_eq!(class(name), Class::Deferred("call"), "{name}");
        }
        assert_eq!(class("GROUP_CALL_START"), Class::Deferred("group-call"));
    }

    /// Round B, M-2 = S2 (a, c): every exempt and every low-value kind is a kind the table defers (a renamed
    /// kind fails here, not silently in the store's eviction), and no kind is both.
    #[test]
    fn the_exempt_and_the_low_value_kinds_are_deferred_kinds_of_the_table() {
        let deferred: BTreeSet<&str> = CSP_TYPES
            .iter()
            .map(|(_, _, c)| *c)
            .chain(ENVELOPE_KINDS.iter().filter_map(|(_, r)| match r {
                Row::Is(c) => Some(*c),
                Row::ByType => None,
            }))
            .filter_map(|c| match c {
                Class::Deferred(kind) => Some(kind),
                _ => None,
            })
            .collect();
        // TH5b B3 maps these three and B4 the three sync kinds; a version-1 row of TH5a may still carry them
        // until the start's re-map takes it, so the eviction order keeps their names (the lists' docs).
        const TH5A_KINDS_MAPPED_IN_B3: [&str; 3] = ["receipt", "read", "sent"];
        const TH5A_KINDS_MAPPED_IN_B4: [&str; 3] = ["settings", "profile", "list-sync"];
        for kind in EXEMPT_KINDS.iter().chain(&LOW_VALUE_KINDS) {
            assert!(
                deferred.contains(kind)
                    || TH5A_KINDS_MAPPED_IN_B3.contains(kind)
                    || TH5A_KINDS_MAPPED_IN_B4.contains(kind),
                "`{kind}` is no deferred kind"
            );
        }
        for kind in TH5A_KINDS_MAPPED_IN_B3 {
            assert!(!deferred.contains(kind), "`{kind}` is deferred again");
            assert!(LOW_VALUE_KINDS.contains(&kind), "`{kind}` left the list");
        }
        for kind in TH5A_KINDS_MAPPED_IN_B4 {
            assert!(!deferred.contains(kind), "`{kind}` is deferred again");
            assert!(EXEMPT_KINDS.contains(&kind), "`{kind}` left the list");
        }
        assert!(
            EXEMPT_KINDS.iter().all(|k| !LOW_VALUE_KINDS.contains(k)),
            "a kind is both exempt and low-value"
        );
    }
}
