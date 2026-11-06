//! THE INCOMING PROPERTIES OF A MESSAGE TYPE (TH6 L3): what the leader's receive steps (`TP/csp.struct.yml:686-873`) ask
//! of a type, as ONE exhaustive match over the pinned `CspE2eMessageType` (45 values, `common.proto`), as
//! `send::props` is for the send. A value the enum gains does not compile here until it has its row.
//!
//! The columns are the spec's `Properties` block of each type (`csp.struct.yml`, `csp-e2e.proto`): `Reflect:
//! Incoming`, `Implicit _direct_ contact creation`, `Delivery receipts`, `Protect against replay`; plus the type's form
//! (a group message in the member or the creator container) and what the leader does with it ([`Action`]). The two
//! columns the send already holds, the blocking exemption and the user profile distribution, are NOT repeated: the
//! steps read them from `send::props` (one place; the spec states them per type, not per direction).
//!
//! Pure: no I/O, no clock, no logging.

use libthreema::protobuf::common::CspE2eMessageType;

/// Whether the steps mark the message's nonce as used (`Protect against replay`, the _Acknowledge_ steps' step 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Replay {
    No,
    Yes,
    /// A delivery receipt: only for a reaction (status 3 or 4), as its body says.
    ByBody,
}

/// Which container a group form is wrapped in (`csp.struct.yml` `group-member-container`, `group-creator-container`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GroupForm {
    /// A 1:1 type, or one with no group form.
    No,
    /// `group-member-container`: the creator's ID, the group id, the inner struct; any member sends it.
    Member,
    /// `group-creator-container`: the group id, the inner struct; the creator is the sender.
    Creator,
}

/// What the leader does with a kept message of the type (brief §2.4 with the critic's changes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Action {
    /// The follower's mapper maps it; reflected, then acked.
    Map,
    /// An event and an ephemeral reflect; no zone, no nonce, no ack (the typing indicator, flags `0x06`).
    Ephemeral,
    /// Group control: a real change is HELD for the phone (C1 as T-M1 shaped it, his TH6-Q1); no change is the spec's
    /// discard.
    GroupControl,
    /// `group-sync-request`: the creator's answer (C7, T-M3).
    CreatorAnswer,
    /// The forward security envelope (step 12): rejected or ignored, never processed (no FS session in Kourier).
    Fs,
    /// Only a special contact sends it (`*3MAPUSH`: web session resume; `*3MAW0RK`: work sync); Kourier has neither a
    /// web nor a work client, so it is the spec's discard from anybody.
    SpecialOnly,
    /// The `empty` message: the spec's discard after the common steps.
    Discard,
}

/// What the leader's receive steps ask of one message type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Incoming {
    /// `Reflect: Incoming`: a kept message of the type is reflected to the other devices (step 23).
    pub(crate) reflect_incoming: bool,
    /// `Implicit _direct_ contact creation` (step 22.4, 22.5.2, 22.6).
    pub(crate) implicit_direct: bool,
    /// `Delivery receipts`: an automatic `received` receipt (step 24), unless the flags hold `0x80`.
    pub(crate) receipt: bool,
    /// `Protect against replay` (the _Acknowledge_ steps' step 3).
    pub(crate) replay: Replay,
    /// The container of a group form.
    pub(crate) group: GroupForm,
    /// What the leader does with a kept one.
    pub(crate) action: Action,
}

/// A row: the six columns in the order of [`Incoming`].
const fn row(
    reflect_incoming: bool,
    implicit_direct: bool,
    receipt: bool,
    replay: Replay,
    group: GroupForm,
    action: Action,
) -> Incoming {
    Incoming {
        reflect_incoming,
        implicit_direct,
        receipt,
        replay,
        group,
        action,
    }
}

/// The incoming properties of `ty`; `None` for `InvalidType`, which no message has. Exhaustive: no wildcard arm.
pub(crate) fn incoming(ty: CspE2eMessageType) -> Option<Incoming> {
    use Action::{CreatorAnswer, Discard, Ephemeral, Fs, GroupControl, Map, SpecialOnly};
    use CspE2eMessageType as T;
    use GroupForm::{Creator, Member, No as OneToOne};
    use Replay::{ByBody, No, Yes};

    Some(match ty {
        T::InvalidType => return None,
        // `empty` (`csp.struct.yml:2912`): not reflected, no contact, replay-protected.
        T::Empty => row(false, false, false, Yes, OneToOne, Discard),
        // 1:1 conversation messages: the only types with an automatic receipt.
        T::Text
        | T::DeprecatedImage
        | T::Location
        | T::DeprecatedAudio
        | T::DeprecatedVideo
        | T::File
        | T::PollSetup => row(true, true, true, Yes, OneToOne, Map),
        T::PollVote => row(true, false, false, Yes, OneToOne, Map),
        // Calls: only the offer creates a direct contact; the ICE candidates are not replay-protected.
        T::CallOffer => row(true, true, false, Yes, OneToOne, Map),
        T::CallAnswer | T::CallHangup | T::CallRinging => {
            row(true, false, false, Yes, OneToOne, Map)
        }
        T::CallIceCandidate => row(true, false, false, No, OneToOne, Map),
        // 1:1 message updates.
        T::DeliveryReceipt => row(true, false, false, ByBody, OneToOne, Map),
        T::TypingIndicator => row(true, false, false, No, OneToOne, Ephemeral),
        T::Reaction | T::EditMessage | T::DeleteMessage => {
            row(true, false, false, Yes, OneToOne, Map)
        }
        // 1:1 contact control (the pictures are TH8's: the mapper drops them as `contact-control`).
        T::ContactSetProfilePicture
        | T::ContactDeleteProfilePicture
        | T::ContactRequestProfilePicture => row(true, false, false, Yes, OneToOne, Map),
        // Group control: the creator's in its container, the leave in the member's.
        T::GroupSetup => row(true, true, false, Yes, Creator, GroupControl),
        T::GroupName | T::GroupSetProfilePicture | T::GroupDeleteProfilePicture => {
            row(true, false, false, Yes, Creator, GroupControl)
        }
        T::GroupLeave => row(true, false, false, Yes, Member, GroupControl),
        T::GroupSyncRequest => row(true, false, false, Yes, Member, CreatorAnswer),
        T::GroupCallStart => row(true, false, false, Yes, Member, Map),
        // Group messages.
        T::GroupText
        | T::GroupLocation
        | T::GroupImage
        | T::GroupAudio
        | T::GroupVideo
        | T::GroupFile
        | T::GroupPollSetup
        | T::GroupPollVote => row(true, false, false, Yes, Member, Map),
        // Group message updates.
        T::GroupDeliveryReceipt => row(true, false, false, ByBody, Member, Map),
        T::GroupEditMessage | T::GroupDeleteMessage | T::GroupReaction => {
            row(true, false, false, Yes, Member, Map)
        }
        // Special: never reflected; the steps decide before a contact is looked at.
        T::ForwardSecurityEnvelope => row(false, false, false, Yes, OneToOne, Fs),
        T::WorkSyncDelta | T::WebSessionResume => {
            row(false, false, false, Yes, OneToOne, SpecialOnly)
        }
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::send::props::{Distribution, Exempt, props};

    /// The literal table: `(value, type, reflect, implicit direct, receipt, replay, form, action)`, one row per value
    /// of the pinned enum, each read off the spec's `Properties` block of the type.
    #[expect(
        clippy::type_complexity,
        reason = "a literal table of the spec's columns"
    )]
    fn table() -> Vec<(
        u8,
        CspE2eMessageType,
        Option<(bool, bool, bool, Replay, GroupForm, Action)>,
    )> {
        use Action::{CreatorAnswer, Discard, Ephemeral, Fs, GroupControl, Map, SpecialOnly};
        use CspE2eMessageType as T;
        use GroupForm::{Creator, Member, No as One};
        use Replay::{ByBody, No, Yes};
        vec![
            (0x00, T::InvalidType, None),
            (
                0xfc,
                T::Empty,
                Some((false, false, false, Yes, One, Discard)),
            ),
            (0x01, T::Text, Some((true, true, true, Yes, One, Map))),
            (
                0x02,
                T::DeprecatedImage,
                Some((true, true, true, Yes, One, Map)),
            ),
            (0x10, T::Location, Some((true, true, true, Yes, One, Map))),
            (
                0x14,
                T::DeprecatedAudio,
                Some((true, true, true, Yes, One, Map)),
            ),
            (
                0x13,
                T::DeprecatedVideo,
                Some((true, true, true, Yes, One, Map)),
            ),
            (0x17, T::File, Some((true, true, true, Yes, One, Map))),
            (0x15, T::PollSetup, Some((true, true, true, Yes, One, Map))),
            (0x16, T::PollVote, Some((true, false, false, Yes, One, Map))),
            (0x60, T::CallOffer, Some((true, true, false, Yes, One, Map))),
            (
                0x61,
                T::CallAnswer,
                Some((true, false, false, Yes, One, Map)),
            ),
            (
                0x62,
                T::CallIceCandidate,
                Some((true, false, false, No, One, Map)),
            ),
            (
                0x63,
                T::CallHangup,
                Some((true, false, false, Yes, One, Map)),
            ),
            (
                0x64,
                T::CallRinging,
                Some((true, false, false, Yes, One, Map)),
            ),
            (
                0x80,
                T::DeliveryReceipt,
                Some((true, false, false, ByBody, One, Map)),
            ),
            (
                0x90,
                T::TypingIndicator,
                Some((true, false, false, No, One, Ephemeral)),
            ),
            (0x82, T::Reaction, Some((true, false, false, Yes, One, Map))),
            (
                0x91,
                T::EditMessage,
                Some((true, false, false, Yes, One, Map)),
            ),
            (
                0x92,
                T::DeleteMessage,
                Some((true, false, false, Yes, One, Map)),
            ),
            (
                0x18,
                T::ContactSetProfilePicture,
                Some((true, false, false, Yes, One, Map)),
            ),
            (
                0x19,
                T::ContactDeleteProfilePicture,
                Some((true, false, false, Yes, One, Map)),
            ),
            (
                0x1a,
                T::ContactRequestProfilePicture,
                Some((true, false, false, Yes, One, Map)),
            ),
            (
                0x4a,
                T::GroupSetup,
                Some((true, true, false, Yes, Creator, GroupControl)),
            ),
            (
                0x4b,
                T::GroupName,
                Some((true, false, false, Yes, Creator, GroupControl)),
            ),
            (
                0x4c,
                T::GroupLeave,
                Some((true, false, false, Yes, Member, GroupControl)),
            ),
            (
                0x50,
                T::GroupSetProfilePicture,
                Some((true, false, false, Yes, Creator, GroupControl)),
            ),
            (
                0x54,
                T::GroupDeleteProfilePicture,
                Some((true, false, false, Yes, Creator, GroupControl)),
            ),
            (
                0x51,
                T::GroupSyncRequest,
                Some((true, false, false, Yes, Member, CreatorAnswer)),
            ),
            (
                0x4f,
                T::GroupCallStart,
                Some((true, false, false, Yes, Member, Map)),
            ),
            (
                0x41,
                T::GroupText,
                Some((true, false, false, Yes, Member, Map)),
            ),
            (
                0x42,
                T::GroupLocation,
                Some((true, false, false, Yes, Member, Map)),
            ),
            (
                0x43,
                T::GroupImage,
                Some((true, false, false, Yes, Member, Map)),
            ),
            (
                0x45,
                T::GroupAudio,
                Some((true, false, false, Yes, Member, Map)),
            ),
            (
                0x44,
                T::GroupVideo,
                Some((true, false, false, Yes, Member, Map)),
            ),
            (
                0x46,
                T::GroupFile,
                Some((true, false, false, Yes, Member, Map)),
            ),
            (
                0x52,
                T::GroupPollSetup,
                Some((true, false, false, Yes, Member, Map)),
            ),
            (
                0x53,
                T::GroupPollVote,
                Some((true, false, false, Yes, Member, Map)),
            ),
            (
                0x81,
                T::GroupDeliveryReceipt,
                Some((true, false, false, ByBody, Member, Map)),
            ),
            (
                0x93,
                T::GroupEditMessage,
                Some((true, false, false, Yes, Member, Map)),
            ),
            (
                0x94,
                T::GroupDeleteMessage,
                Some((true, false, false, Yes, Member, Map)),
            ),
            (
                0x83,
                T::GroupReaction,
                Some((true, false, false, Yes, Member, Map)),
            ),
            (
                0xa0,
                T::ForwardSecurityEnvelope,
                Some((false, false, false, Yes, One, Fs)),
            ),
            (
                0xfd,
                T::WorkSyncDelta,
                Some((false, false, false, Yes, One, SpecialOnly)),
            ),
            (
                0xfe,
                T::WebSessionResume,
                Some((false, false, false, Yes, One, SpecialOnly)),
            ),
        ]
    }

    /// The `(NAME, value)` lines of `enum CspE2eMessageType { ... }` in the pinned `common.proto`, read at run time.
    fn proto_values() -> BTreeSet<u8> {
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
                let (_, value) = line.trim().strip_suffix(';')?.split_once(" = ")?;
                u8::from_str_radix(value.trim().strip_prefix("0x")?, 16).ok()
            })
            .collect()
    }

    /// Every row against the literal table, and the table covers exactly the values of the pinned `common.proto`.
    #[test]
    fn every_type_has_its_incoming_row_and_the_table_is_the_protos() {
        let table = table();
        assert_eq!(table.len(), 45, "one literal row per value of the enum");
        for (number, ty, expected) in &table {
            assert_eq!(*ty as i32, i32::from(*number), "{ty:?}: its number");
            let got = incoming(*ty).map(|p| {
                (
                    p.reflect_incoming,
                    p.implicit_direct,
                    p.receipt,
                    p.replay,
                    p.group,
                    p.action,
                )
            });
            assert_eq!(got, *expected, "{ty:?}");
        }
        let proto = proto_values();
        let mine: BTreeSet<u8> = table.iter().map(|(n, _, _)| *n).collect();
        assert_eq!(proto.len(), 45, "the proto's values");
        assert_eq!(mine, proto, "the literal table and the proto");
        for number in 0..=u8::MAX {
            let known = CspE2eMessageType::try_from(i32::from(number)).is_ok();
            assert_eq!(known, proto.contains(&number), "byte {number:#04x}");
        }
    }

    /// The incoming table and the send table agree where both speak: the same types have a row, a group form is
    /// exactly a type the send rule for a Gateway creator names as a group type or a group control, a type with an
    /// automatic receipt distributes the profile, and the columns the steps read from `send::props` are the spec's for
    /// the receive (the exemptions of group control, the dedicated steps of the setup).
    #[test]
    fn the_incoming_table_agrees_with_the_send_table() {
        use crate::send::props::GatewayCreator;
        use CspE2eMessageType as T;
        for (_, ty, _) in table() {
            let (Some(inc), send) = (incoming(ty), props(ty)) else {
                assert_eq!(props(ty), None, "{ty:?}: no row in either");
                continue;
            };
            let send = send.expect("a send row");
            // A member-container type is one the send's Gateway rule applies to (or a leave / sync request).
            let gateway_group = send.gateway_creator != GatewayCreator::NotApplicable;
            assert_eq!(
                inc.group == GroupForm::Member,
                gateway_group,
                "{ty:?}: member form"
            );
            if inc.receipt {
                assert_eq!(send.profile_distribution, Distribution::Yes, "{ty:?}");
                assert_eq!(inc.group, GroupForm::No, "{ty:?}: no receipt to a group");
            }
            if inc.action == Action::GroupControl && ty != T::GroupSetup {
                assert_eq!(send.exempt_from_blocking, Exempt::Yes, "{ty:?}");
            }
        }
        assert_eq!(
            props(T::GroupSetup).expect("setup").exempt_from_blocking,
            Exempt::DedicatedSteps
        );
        // The spec's group poll setup and vote are exempt; the texts are not.
        assert_eq!(
            props(T::GroupPollSetup).expect("p").exempt_from_blocking,
            Exempt::Yes
        );
        assert_eq!(
            props(T::GroupText).expect("t").exempt_from_blocking,
            Exempt::No
        );
        // The only ephemeral type is the one the server never acks (0x04).
        for (_, ty, _) in table() {
            if let Some(inc) = incoming(ty) {
                let no_ack =
                    props(ty).is_some_and(|p| p.flags & crate::send::props::NO_SERVER_ACK != 0);
                assert_eq!(inc.action == Action::Ephemeral, no_ack, "{ty:?}");
            }
        }
    }

    /// The match has no wildcard arm, read from the source (a wildcard would pass every runtime check above).
    #[test]
    fn the_incoming_match_has_no_wildcard_arm() {
        let source = include_str!("props.rs");
        let start = source
            .find("pub(crate) fn incoming(")
            .expect("the function");
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
        for (_, ty, _) in table() {
            let name = format!("T::{ty:?}");
            assert!(function.contains(&name), "{name} has no arm");
        }
    }
}
