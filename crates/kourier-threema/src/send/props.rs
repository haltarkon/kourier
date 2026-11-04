//! THE PROPERTIES OF A MESSAGE TYPE (TH6a S2): what the send steps ask of a type, as ONE exhaustive match over the
//! pinned `CspE2eMessageType` (45 values, `common.proto`). A value the enum gains does not compile here until it has
//! its row.
//!
//! The rows are Desktop's `MESSAGE_TYPE_PROPERTIES` (`protocol/index.ts`, 43 rows), each checked against the pinned
//! spec's `Properties` block of the type (`csp.struct.yml`, `csp-e2e.proto`); the CSP flags are the spec's
//! `Flags` list (Desktop picks them per task, not per type). Where Desktop and the spec differ, or one of them has no
//! row, the row says so:
//!
//! - `DeprecatedImage`, `DeprecatedAudio`, `DeprecatedVideo` and `GroupImage`, `GroupAudio`, `GroupVideo`: Desktop
//!   reflects them outgoing, the spec says `N/A` (no app sends them any more). TH6a never sends them; the row is
//!   Desktop's.
//! - `ForwardSecurityEnvelope`: the spec states no properties ("not currently specified", Desktop's own note); Desktop's
//!   defaults, no flags. Kourier sends none (the steps return the inner message when forward security is unsupported).
//! - `Empty`: Desktop has no row; the row is the spec's (`csp.struct.yml`, `empty`).
//! - `InvalidType`: no row anywhere. `props` answers `None`: it is no message.
//! - THE GROUP MARKER `0x10`: no group row carries it, as the spec's lists say. The pinned spec and libthreema call it
//!   reserved (formerly the group message marker) and set it nowhere, and no receive path reads it; Desktop sets it on
//!   its group conversation messages, and its own doc of the flag (`flags.ts:31-38`) says only iOS uses it, for the push
//!   fallback text. Kept per the spec's lists, a [U] for RT2; switching is one row per type here (`GroupText`,
//!   `GroupLocation`, `GroupFile`, `GroupPollSetup` and the deprecated media), no schema change, and a row already
//!   sealed keeps its flags.
//! - `NO_SERVER_ACK` (`0x04`, the typing indicator): the chat server never acks such a message, so PREPARE refuses
//!   the type (`SendRefused::NotQueued`, P-L1): it is volatile and goes as a frame with no row (TH7).
//!
//! Pure: no I/O, no clock, no logging.

use libthreema::protobuf::common::CspE2eMessageType;

/// Whether a message of the type distributes the user's profile (nickname) to its receiver. A type that decides by
/// its content cannot be answered by the type alone: the one who builds the message decides (L8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Distribution {
    No,
    Yes,
    /// A delivery receipt distributes only for a reaction (status 3 or 4), a call answer only when it accepts, a
    /// forward security envelope as its content does.
    ByBody,
}

/// Whether a message of the type goes to a receiver the user blocked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Exempt {
    No,
    Yes,
    /// `group-setup`: the spec's dedicated blocking steps decide by its content.
    DedicatedSteps,
}

/// Whether a group message of the type goes to a group creator that is a Threema Gateway ID (`*...`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GatewayCreator {
    /// A 1:1 type, or a group control the creator sends itself.
    NotApplicable,
    /// Only when the group name starts with the cloud emoji ("capture is enabled").
    IfCaptured,
    /// Always (`group-leave`, `group-sync-request`).
    Always,
}

/// What the send steps ask of one message type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Props {
    /// The user profile distribution of the type.
    pub(crate) profile_distribution: Distribution,
    /// The blocking exemption of the type.
    pub(crate) exempt_from_blocking: Exempt,
    /// An outgoing message of the type is reflected to the other devices.
    pub(crate) reflect_outgoing: bool,
    /// A reflected outgoing message of the type is followed by the `Sent` update.
    pub(crate) reflect_sent_update: bool,
    /// The CSP message flags of the type (the spec's `Flags`: `0x01` push, `0x02` no server queuing, `0x04` no
    /// server acknowledgement, `0x20` short-lived server queuing).
    pub(crate) flags: u8,
    /// The Threema Gateway group creator rule of the type.
    pub(crate) gateway_creator: GatewayCreator,
}

/// A row: the six properties in the order of [`Props`].
const fn row(
    profile_distribution: Distribution,
    exempt_from_blocking: Exempt,
    reflect_outgoing: bool,
    reflect_sent_update: bool,
    flags: u8,
    gateway_creator: GatewayCreator,
) -> Props {
    Props {
        profile_distribution,
        exempt_from_blocking,
        reflect_outgoing,
        reflect_sent_update,
        flags,
        gateway_creator,
    }
}

/// Send a push notification.
const PUSH: u8 = 0x01;
/// No server acknowledgement: the chat server never answers a message with this flag (`csp.struct.yml`, the bundled
/// send steps add its id to the pending acks only without it). PREPARE refuses a type that holds it (`NotQueued`).
pub(crate) const NO_SERVER_ACK: u8 = 0x04;
/// No server queuing and no server acknowledgement (the typing indicator).
const NO_QUEUE_NO_ACK: u8 = 0x02 | NO_SERVER_ACK;
/// Short-lived server queuing.
const SHORT_LIVED: u8 = 0x20;

/// The properties of `ty`; `None` for `InvalidType`, which no message has (module doc). Exhaustive: no wildcard arm.
pub(crate) fn props(ty: CspE2eMessageType) -> Option<Props> {
    use CspE2eMessageType as T;
    use Distribution::{ByBody, No, Yes};
    use Exempt::{DedicatedSteps, No as NotExempt, Yes as Exempted};
    use GatewayCreator::{Always, IfCaptured, NotApplicable};

    Some(match ty {
        T::InvalidType => return None,
        // The spec's own row (Desktop has none).
        T::Empty => row(No, Exempted, false, false, 0, NotApplicable),
        // 1:1 conversation messages.
        T::Text
        | T::DeprecatedImage
        | T::Location
        | T::DeprecatedAudio
        | T::DeprecatedVideo
        | T::File
        | T::PollSetup => row(Yes, NotExempt, true, true, PUSH, NotApplicable),
        T::PollVote => row(Yes, NotExempt, true, false, 0, NotApplicable),
        T::CallOffer => row(
            Yes,
            NotExempt,
            true,
            false,
            PUSH | SHORT_LIVED,
            NotApplicable,
        ),
        T::CallAnswer => row(
            ByBody,
            NotExempt,
            true,
            false,
            PUSH | SHORT_LIVED,
            NotApplicable,
        ),
        T::CallIceCandidate => row(
            No,
            NotExempt,
            false,
            false,
            PUSH | SHORT_LIVED,
            NotApplicable,
        ),
        T::CallHangup => row(No, NotExempt, true, false, PUSH, NotApplicable),
        T::CallRinging => row(
            No,
            NotExempt,
            true,
            false,
            PUSH | SHORT_LIVED,
            NotApplicable,
        ),
        // 1:1 message updates.
        T::DeliveryReceipt => row(ByBody, NotExempt, true, false, 0, NotApplicable),
        T::TypingIndicator => row(No, NotExempt, false, false, NO_QUEUE_NO_ACK, NotApplicable),
        T::Reaction => row(Yes, NotExempt, true, false, PUSH, NotApplicable),
        T::EditMessage | T::DeleteMessage => row(No, NotExempt, true, false, PUSH, NotApplicable),
        // 1:1 contact control.
        T::ContactSetProfilePicture | T::ContactDeleteProfilePicture => {
            row(No, NotExempt, true, false, 0, NotApplicable)
        }
        T::ContactRequestProfilePicture => row(No, NotExempt, false, false, 0, NotApplicable),
        // Group control.
        T::GroupSetup => row(Yes, DedicatedSteps, true, false, 0, NotApplicable),
        T::GroupName | T::GroupSetProfilePicture | T::GroupDeleteProfilePicture => {
            row(No, Exempted, true, false, 0, NotApplicable)
        }
        T::GroupLeave | T::GroupSyncRequest => row(No, Exempted, true, false, 0, Always),
        T::GroupCallStart => row(Yes, Exempted, true, false, PUSH, IfCaptured),
        // Group messages.
        T::GroupText
        | T::GroupLocation
        | T::GroupImage
        | T::GroupAudio
        | T::GroupVideo
        | T::GroupFile => row(Yes, NotExempt, true, true, PUSH, IfCaptured),
        T::GroupPollSetup => row(Yes, Exempted, true, true, PUSH, IfCaptured),
        T::GroupPollVote => row(Yes, Exempted, true, false, 0, IfCaptured),
        // Group message updates.
        T::GroupDeliveryReceipt => row(ByBody, NotExempt, true, false, 0, IfCaptured),
        T::GroupEditMessage | T::GroupDeleteMessage => {
            row(No, NotExempt, true, false, PUSH, IfCaptured)
        }
        T::GroupReaction => row(Yes, NotExempt, true, false, PUSH, IfCaptured),
        // Special.
        T::ForwardSecurityEnvelope => row(ByBody, NotExempt, false, false, 0, NotApplicable),
        T::WorkSyncDelta => row(No, Exempted, false, false, 0, NotApplicable),
        T::WebSessionResume => row(No, Exempted, false, false, SHORT_LIVED, NotApplicable),
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::Distribution::{ByBody, No, Yes};
    use super::Exempt::{DedicatedSteps, No as NotExempt, Yes as Exempted};
    use super::GatewayCreator::{Always, IfCaptured, NotApplicable};
    use super::*;

    /// A literal row, written field by field (not through [`row`], so the test does not share the code's helper).
    fn p(
        profile_distribution: Distribution,
        exempt_from_blocking: Exempt,
        reflect_outgoing: bool,
        reflect_sent_update: bool,
        flags: u8,
        gateway_creator: GatewayCreator,
    ) -> Option<Props> {
        Some(Props {
            profile_distribution,
            exempt_from_blocking,
            reflect_outgoing,
            reflect_sent_update,
            flags,
            gateway_creator,
        })
    }

    /// EVERY type of the pinned enum with its row, written from Desktop's `MESSAGE_TYPE_PROPERTIES` (43 rows) and the
    /// spec's `Properties` blocks and `Flags` lists (`Empty`: the spec's block alone; `InvalidType`: no row). The
    /// number is the type's value in `common.proto`.
    fn table() -> Vec<(u8, CspE2eMessageType, Option<Props>)> {
        use CspE2eMessageType as T;
        vec![
            (0x00, T::InvalidType, None),
            (
                0xfc,
                T::Empty,
                p(No, Exempted, false, false, 0, NotApplicable),
            ),
            // 1:1 messages.
            (
                0x01,
                T::Text,
                p(Yes, NotExempt, true, true, 0x01, NotApplicable),
            ),
            (
                0x02,
                T::DeprecatedImage,
                p(Yes, NotExempt, true, true, 0x01, NotApplicable),
            ),
            (
                0x10,
                T::Location,
                p(Yes, NotExempt, true, true, 0x01, NotApplicable),
            ),
            (
                0x14,
                T::DeprecatedAudio,
                p(Yes, NotExempt, true, true, 0x01, NotApplicable),
            ),
            (
                0x13,
                T::DeprecatedVideo,
                p(Yes, NotExempt, true, true, 0x01, NotApplicable),
            ),
            (
                0x17,
                T::File,
                p(Yes, NotExempt, true, true, 0x01, NotApplicable),
            ),
            (
                0x15,
                T::PollSetup,
                p(Yes, NotExempt, true, true, 0x01, NotApplicable),
            ),
            (
                0x16,
                T::PollVote,
                p(Yes, NotExempt, true, false, 0x00, NotApplicable),
            ),
            (
                0x60,
                T::CallOffer,
                p(Yes, NotExempt, true, false, 0x21, NotApplicable),
            ),
            (
                0x61,
                T::CallAnswer,
                p(ByBody, NotExempt, true, false, 0x21, NotApplicable),
            ),
            (
                0x62,
                T::CallIceCandidate,
                p(No, NotExempt, false, false, 0x21, NotApplicable),
            ),
            (
                0x63,
                T::CallHangup,
                p(No, NotExempt, true, false, 0x01, NotApplicable),
            ),
            (
                0x64,
                T::CallRinging,
                p(No, NotExempt, true, false, 0x21, NotApplicable),
            ),
            // 1:1 message updates.
            (
                0x80,
                T::DeliveryReceipt,
                p(ByBody, NotExempt, true, false, 0x00, NotApplicable),
            ),
            (
                0x90,
                T::TypingIndicator,
                p(No, NotExempt, false, false, 0x06, NotApplicable),
            ),
            (
                0x82,
                T::Reaction,
                p(Yes, NotExempt, true, false, 0x01, NotApplicable),
            ),
            (
                0x91,
                T::EditMessage,
                p(No, NotExempt, true, false, 0x01, NotApplicable),
            ),
            (
                0x92,
                T::DeleteMessage,
                p(No, NotExempt, true, false, 0x01, NotApplicable),
            ),
            // 1:1 contact control.
            (
                0x18,
                T::ContactSetProfilePicture,
                p(No, NotExempt, true, false, 0x00, NotApplicable),
            ),
            (
                0x19,
                T::ContactDeleteProfilePicture,
                p(No, NotExempt, true, false, 0x00, NotApplicable),
            ),
            (
                0x1a,
                T::ContactRequestProfilePicture,
                p(No, NotExempt, false, false, 0x00, NotApplicable),
            ),
            // Group control.
            (
                0x4a,
                T::GroupSetup,
                p(Yes, DedicatedSteps, true, false, 0x00, NotApplicable),
            ),
            (
                0x4b,
                T::GroupName,
                p(No, Exempted, true, false, 0x00, NotApplicable),
            ),
            (
                0x4c,
                T::GroupLeave,
                p(No, Exempted, true, false, 0x00, Always),
            ),
            (
                0x50,
                T::GroupSetProfilePicture,
                p(No, Exempted, true, false, 0x00, NotApplicable),
            ),
            (
                0x54,
                T::GroupDeleteProfilePicture,
                p(No, Exempted, true, false, 0x00, NotApplicable),
            ),
            (
                0x51,
                T::GroupSyncRequest,
                p(No, Exempted, true, false, 0x00, Always),
            ),
            (
                0x4f,
                T::GroupCallStart,
                p(Yes, Exempted, true, false, 0x01, IfCaptured),
            ),
            // Group messages.
            (
                0x41,
                T::GroupText,
                p(Yes, NotExempt, true, true, 0x01, IfCaptured),
            ),
            (
                0x42,
                T::GroupLocation,
                p(Yes, NotExempt, true, true, 0x01, IfCaptured),
            ),
            (
                0x43,
                T::GroupImage,
                p(Yes, NotExempt, true, true, 0x01, IfCaptured),
            ),
            (
                0x45,
                T::GroupAudio,
                p(Yes, NotExempt, true, true, 0x01, IfCaptured),
            ),
            (
                0x44,
                T::GroupVideo,
                p(Yes, NotExempt, true, true, 0x01, IfCaptured),
            ),
            (
                0x46,
                T::GroupFile,
                p(Yes, NotExempt, true, true, 0x01, IfCaptured),
            ),
            (
                0x52,
                T::GroupPollSetup,
                p(Yes, Exempted, true, true, 0x01, IfCaptured),
            ),
            (
                0x53,
                T::GroupPollVote,
                p(Yes, Exempted, true, false, 0x00, IfCaptured),
            ),
            // Group message updates.
            (
                0x81,
                T::GroupDeliveryReceipt,
                p(ByBody, NotExempt, true, false, 0x00, IfCaptured),
            ),
            (
                0x93,
                T::GroupEditMessage,
                p(No, NotExempt, true, false, 0x01, IfCaptured),
            ),
            (
                0x94,
                T::GroupDeleteMessage,
                p(No, NotExempt, true, false, 0x01, IfCaptured),
            ),
            (
                0x83,
                T::GroupReaction,
                p(Yes, NotExempt, true, false, 0x01, IfCaptured),
            ),
            // Special.
            (
                0xa0,
                T::ForwardSecurityEnvelope,
                p(ByBody, NotExempt, false, false, 0x00, NotApplicable),
            ),
            (
                0xfd,
                T::WorkSyncDelta,
                p(No, Exempted, false, false, 0x00, NotApplicable),
            ),
            (
                0xfe,
                T::WebSessionResume,
                p(No, Exempted, false, false, 0x20, NotApplicable),
            ),
        ]
    }

    /// The `(NAME, value)` lines of `enum CspE2eMessageType { ... }` in the pinned `common.proto`.
    fn proto_values() -> Vec<(String, u8)> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../third_party/libthreema/threema-protocols/src/common.proto");
        let text = std::fs::read_to_string(path).expect("the pinned common.proto");
        let start = text
            .find("enum CspE2eMessageType")
            .expect("the enum in common.proto");
        let body = &text[start..];
        let body = &body[..body.find("\n}").expect("the enum's end")];
        body.lines()
            .filter_map(|line| {
                let (name, value) = line.trim().strip_suffix(';')?.split_once(" = ")?;
                let value = value.trim().strip_prefix("0x")?;
                Some((name.trim().to_owned(), u8::from_str_radix(value, 16).ok()?))
            })
            .collect()
    }

    /// Every row against the literal table: all 45 values of the enum, one row each.
    #[test]
    fn every_type_has_the_row_of_desktops_table_and_the_spec() {
        let table = table();
        assert_eq!(table.len(), 45, "one literal row per value of the enum");
        for (number, ty, expected) in &table {
            assert_eq!(*ty as i32, i32::from(*number), "{ty:?}: its number");
            assert_eq!(props(*ty), *expected, "{ty:?}");
        }
    }

    /// The literal table covers exactly the values of the pinned `common.proto`, and every other byte is no type
    /// (so a value the proto gains, or a row twice, stops here before it stops anywhere else).
    #[test]
    fn the_table_holds_exactly_the_values_of_the_pinned_proto() {
        let proto: BTreeSet<u8> = proto_values().into_iter().map(|(_, v)| v).collect();
        let mine: BTreeSet<u8> = table().iter().map(|(n, _, _)| *n).collect();
        assert_eq!(proto.len(), 45, "the proto's values");
        assert_eq!(mine, proto, "the literal table and the proto");
        assert_eq!(table().len(), mine.len(), "no value twice");
        for number in 0..=u8::MAX {
            let known = CspE2eMessageType::try_from(i32::from(number)).is_ok();
            assert_eq!(known, proto.contains(&number), "byte {number:#04x}");
        }
        // Only the invalid type has no row.
        let rowless: Vec<_> = table()
            .into_iter()
            .filter(|(_, _, row)| row.is_none())
            .map(|(n, _, _)| n)
            .collect();
        assert_eq!(rowless, vec![0x00]);
    }

    /// The rows the sends of TH6a depend on, one by one, so a change to one is read at a glance: text, group text, a
    /// delivery receipt (L8), a group name, a group leave, the forward security envelope.
    #[test]
    fn the_rows_the_send_core_depends_on() {
        use CspE2eMessageType as T;
        let text = props(T::Text).expect("text");
        assert_eq!(text.profile_distribution, Yes);
        assert_eq!(text.exempt_from_blocking, NotExempt);
        assert!(text.reflect_outgoing && text.reflect_sent_update);
        assert_eq!(text.flags, 0x01);
        assert_eq!(text.gateway_creator, NotApplicable);

        let group = props(T::GroupText).expect("group text");
        assert_eq!(group.gateway_creator, IfCaptured);
        assert!(group.reflect_outgoing && group.reflect_sent_update);
        assert_eq!(group.flags, 0x01, "the group marker 0x10 is reserved");

        // L8: a delivery receipt decides by its status, so the type cannot say; it is reflected without a `Sent`
        // update and carries no flag.
        let receipt = props(T::DeliveryReceipt).expect("receipt");
        assert_eq!(receipt.profile_distribution, ByBody);
        assert!(receipt.reflect_outgoing && !receipt.reflect_sent_update);
        assert_eq!(receipt.flags, 0);
        assert_eq!(
            props(T::GroupDeliveryReceipt)
                .expect("group receipt")
                .profile_distribution,
            ByBody
        );

        // The group controls go to a blocked member; a leave reaches a Gateway creator always.
        let name = props(T::GroupName).expect("group name");
        assert_eq!(name.exempt_from_blocking, Exempted);
        assert!(name.reflect_outgoing && !name.reflect_sent_update);
        assert_eq!(name.profile_distribution, No);
        let leave = props(T::GroupLeave).expect("group leave");
        assert_eq!(leave.exempt_from_blocking, Exempted);
        assert_eq!(leave.gateway_creator, Always);
        // L8: `exempt_from_blocking` covers the dedicated steps of a group setup.
        assert_eq!(
            props(T::GroupSetup).expect("setup").exempt_from_blocking,
            DedicatedSteps
        );

        // The forward security envelope is never reflected and sets no flag; the profile follows its content.
        let fs = props(T::ForwardSecurityEnvelope).expect("fs");
        assert!(!fs.reflect_outgoing && !fs.reflect_sent_update);
        assert_eq!(fs.profile_distribution, ByBody);
        assert_eq!(fs.flags, 0);

        // The typing indicator is not queued and not acknowledged (0x02 | 0x04) and is not reflected.
        let typing = props(T::TypingIndicator).expect("typing");
        assert_eq!(typing.flags, 0x06);
        assert!(!typing.reflect_outgoing);
        assert_eq!(props(T::InvalidType), None);
    }

    /// The `Sent` update follows exactly the types whose row says so: the conversation messages that show a tick
    /// (text, location, file, poll setup, in a chat and in a group), never a control, a receipt or a vote.
    #[test]
    fn only_the_conversation_messages_get_a_sent_update() {
        let with_update: BTreeSet<u8> = table()
            .iter()
            .filter(|(_, ty, _)| props(*ty).is_some_and(|p| p.reflect_sent_update))
            .map(|(n, _, _)| *n)
            .collect();
        let expected: BTreeSet<u8> = [
            0x01, 0x02, 0x10, 0x13, 0x14, 0x15, 0x17, // 1:1
            0x41, 0x42, 0x43, 0x44, 0x45, 0x46, 0x52, // group
        ]
        .into_iter()
        .collect();
        assert_eq!(with_update, expected);
    }

    /// The match has no wildcard arm: a value the enum gains must not compile there until it has a row. Read from the
    /// source, since a wildcard would pass every runtime check of the table above.
    #[test]
    fn the_match_has_no_wildcard_arm() {
        let source = include_str!("props.rs");
        let start = source.find("pub(crate) fn props(").expect("the function");
        let end = source[start..]
            .find("\n#[cfg(test)]")
            .expect("the test block")
            + start;
        let function = &source[start..end];
        for line in function.lines() {
            let line = line.trim_start();
            assert!(
                !(line.starts_with("_ =>")
                    || line.starts_with("other =>")
                    || line.starts_with("_ |")),
                "a wildcard arm: {line}"
            );
        }
        assert!(function.contains("match ty {"));
        // Every variant of the enum is named in the function (45 values: the table's names).
        for (_, ty, _) in table() {
            let name = format!("T::{ty:?}");
            assert!(function.contains(&name), "{name} has no arm");
        }
    }
}
