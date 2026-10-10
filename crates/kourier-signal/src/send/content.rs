//! Builders of the `Content`s Kourier sends (§6.7 step 1; Desktop's `Message.toProto` in
//! `SD/ts/textsecure/SendMessage.preload.ts:380-717`, `getDataOrEditMessage`, `getSentMessage`,
//! `syncReadMessages`, `getDeleteForMeSyncMessage`, `#sendReceiptMessage`, the typing message of
//! `conversations.preload.ts:1457-1545` and `addPniSignatureMessageToProto`).
//!
//! Modifiers (S13) use the fields Desktop's jobs set: a reaction
//! (`SD/ts/jobs/helpers/sendReaction.preload.ts`) carries the chat's timer, its version in a
//! 1:1 chat and our profile key; a delete for everyone
//! (`sendDeleteForEveryone.preload.ts`) only the profile key; an edit
//! (`sendNormalMessage.preload.ts` with `targetTimestampForEdit`) is a whole new `DataMessage`
//! inside an `EditMessage` that names the previous revision; a 1:1 timer change
//! (`sendDirectExpirationTimerUpdate.preload.ts`) is a `DataMessage` with the
//! `EXPIRATION_TIMER_UPDATE` flag, the new timer and its version.
//!
//! Service ids go out in their binary form only (Desktop does on staging and the mock); the
//! padding of sync messages is Desktop's random 1-512 bytes, the 80-byte block padding of every
//! `Content` is applied by the sender ([`crate::padding::pad`]).

use kourier_model::{RangeKind, Text, Utf16Map};
use libsignal_core::{Aci, ServiceId};
use rand::Rng as _;
use uuid::Uuid;

use super::media::PreparedBody;
use crate::db::Account;
use crate::error::{Error, Result};
use crate::proto::{self, body_range::AssociatedValue, body_range::Style};

/// `DataMessage.ProtocolVersion.MENTIONS`: required once a body (or a quote) has ranges.
const PROTOCOL_VERSION_MENTIONS: u32 = 6;
/// `DataMessage.Flags.EXPIRATION_TIMER_UPDATE`.
pub(crate) const FLAG_EXPIRATION_TIMER_UPDATE: u32 = 2;

/// The parts of a 1:1 message.
#[derive(Clone, Copy, Debug)]
pub(crate) struct TextMessage<'a> {
    /// Body, ranges and uploaded attachments ([`super::media`]).
    pub(crate) body: &'a PreparedBody,
    /// The sent timestamp (also the envelope's).
    pub(crate) timestamp: u64,
    /// Our profile key (sent with every message, §6.7 M1 rule).
    pub(crate) profile_key: &'a [u8; 32],
    /// The chat's disappearing timer in seconds, 0 = off.
    pub(crate) expire_timer: u32,
    /// The chat's timer version (Desktop starts at 1).
    pub(crate) expire_timer_version: u32,
    /// The message this one replies to.
    pub(crate) quote: Option<&'a proto::data_message::Quote>,
}

/// The `DataMessage` of a 1:1 message: body with UTF-16 ranges, attachments, timestamp,
/// profile key, timer and its version (always sent in 1:1 chats, else the peer's timer breaks).
pub(crate) fn text_message(message: &TextMessage<'_>) -> proto::DataMessage {
    let body = message.body;
    let ranges =
        !body.ranges.is_empty() || message.quote.is_some_and(|q| !q.body_ranges.is_empty());
    proto::DataMessage {
        body: body.text.clone(),
        required_protocol_version: ranges.then_some(PROTOCOL_VERSION_MENTIONS),
        quote: message.quote.cloned(),
        body_ranges: body.ranges.clone(),
        attachments: body.attachments.clone(),
        timestamp: Some(message.timestamp),
        profile_key: Some(message.profile_key.to_vec()),
        expire_timer: Some(message.expire_timer),
        expire_timer_version: Some(message.expire_timer_version.max(1)),
        ..Default::default()
    }
}

/// The group of a group message (`GroupContextV2`, `SD/ts/jobs/helpers/sendNormalMessage.preload.ts`
/// captures the revision at enqueue; Kourier at send, which the receivers accept the same way).
#[derive(Clone, Copy)]
pub(crate) struct GroupContext<'a> {
    /// The group's master key (secret).
    pub(crate) master_key: &'a [u8; 32],
    /// The revision we know.
    pub(crate) revision: u32,
}

/// `message` as a message of `group`: with the group context and the group's timer, without a
/// timer version (`getGroupV2Info`; Desktop passes `expireTimerVersion` for 1:1 chats only).
pub(crate) fn in_group(
    mut message: proto::DataMessage,
    group: GroupContext<'_>,
) -> proto::DataMessage {
    message.expire_timer_version = None;
    message.group_v2 = Some(proto::GroupContextV2 {
        master_key: Some(group.master_key.to_vec()),
        revision: Some(group.revision),
        group_change: None,
    });
    message
}

/// A reaction to send (or to take back).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Reaction<'a> {
    /// The emoji; the one being removed when `remove` is set (Desktop's `emojiToRemove`).
    pub(crate) emoji: &'a str,
    /// Takes the reaction back.
    pub(crate) remove: bool,
    /// Who wrote the message reacted to (our own ACI for our messages).
    pub(crate) target_author: Aci,
    /// A sent timestamp of that message (any revision).
    pub(crate) target_timestamp: u64,
}

/// The `DataMessage` of a reaction in a 1:1 chat (`sendReaction.preload.ts`): the reaction, our
/// profile key and the chat's timer with its version, so the reaction does not reset the
/// peer's timer. For a group, wrap it with [`in_group`].
pub(crate) fn reaction_message(
    reaction: &Reaction<'_>,
    timestamp: u64,
    profile_key: &[u8; 32],
    expire_timer: u32,
    expire_timer_version: u32,
) -> proto::DataMessage {
    proto::DataMessage {
        reaction: Some(proto::data_message::Reaction {
            emoji: Some(reaction.emoji.to_owned()),
            remove: Some(reaction.remove),
            target_author_aci: None,
            target_author_aci_binary: Some(Uuid::from(reaction.target_author).as_bytes().to_vec()),
            target_sent_timestamp: Some(reaction.target_timestamp),
        }),
        timestamp: Some(timestamp),
        profile_key: Some(profile_key.to_vec()),
        expire_timer: Some(expire_timer),
        expire_timer_version: Some(expire_timer_version.max(1)),
        ..Default::default()
    }
}

/// The `DataMessage` of a delete for everyone of our own message sent at `target_timestamp`
/// (`sendDeleteForEveryone.preload.ts`: the delete and our profile key, no timer).
pub(crate) fn delete_message(
    target_timestamp: u64,
    timestamp: u64,
    profile_key: &[u8; 32],
) -> proto::DataMessage {
    proto::DataMessage {
        delete: Some(proto::data_message::Delete {
            target_sent_timestamp: Some(target_timestamp),
        }),
        timestamp: Some(timestamp),
        profile_key: Some(profile_key.to_vec()),
        ..Default::default()
    }
}

/// The `DataMessage` of a group admin's delete of `target_author`'s message
/// (`DataMessage.adminDelete`, Desktop's `isAdminDelete`).
pub(crate) fn admin_delete_message(
    target_author: Aci,
    target_timestamp: u64,
    timestamp: u64,
    profile_key: &[u8; 32],
) -> proto::DataMessage {
    proto::DataMessage {
        admin_delete: Some(proto::data_message::AdminDelete {
            target_author_aci_binary: Some(Uuid::from(target_author).as_bytes().to_vec()),
            target_sent_timestamp: Some(target_timestamp),
        }),
        timestamp: Some(timestamp),
        profile_key: Some(profile_key.to_vec()),
        ..Default::default()
    }
}

/// The `DataMessage` that changes a 1:1 chat's disappearing-message timer
/// (`sendDirectExpirationTimerUpdate.preload.ts`): the flag, the new timer (0 = off), the
/// version this change has, and our profile key.
pub(crate) fn timer_update_message(
    timestamp: u64,
    profile_key: &[u8; 32],
    expire_timer: u32,
    expire_timer_version: u32,
) -> proto::DataMessage {
    proto::DataMessage {
        flags: Some(FLAG_EXPIRATION_TIMER_UPDATE),
        timestamp: Some(timestamp),
        profile_key: Some(profile_key.to_vec()),
        expire_timer: Some(expire_timer),
        expire_timer_version: Some(expire_timer_version.max(1)),
        ..Default::default()
    }
}

/// An `EditMessage` (`getDataOrEditMessage`): `message` is the new revision, a whole
/// `DataMessage` with its own timestamp; `target_timestamp` the sent timestamp of the revision
/// it replaces (`SD/ts/util/editHelpers.std.ts`).
pub(crate) fn edit_message(
    target_timestamp: u64,
    message: proto::DataMessage,
) -> proto::EditMessage {
    proto::EditMessage {
        target_sent_timestamp: Some(target_timestamp),
        data_message: Some(message),
    }
}

/// A `Content` around an `EditMessage`.
pub(crate) fn edit_content(edit: proto::EditMessage) -> proto::Content {
    proto::Content {
        content: Some(proto::content::Content::EditMessage(edit)),
        ..Default::default()
    }
}

/// The attachment a quote names: the quoted message's first one, with its thumbnail when one
/// was uploaded (`SD/ts/util/makeQuote.preload.ts`, `uploadMessageQuote`).
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct QuotedFile {
    /// Its content type.
    pub(crate) content_type: Option<String>,
    /// Its file name.
    pub(crate) file_name: Option<String>,
    /// The uploaded thumbnail.
    pub(crate) thumbnail: Option<proto::AttachmentPointer>,
}

/// `DataMessage.quote`: the quoted message by its sent timestamp and author, with the text and
/// ranges the reply showed (`Message.toProto`, type NORMAL).
pub(crate) fn quote(
    id: u64,
    author: Aci,
    text: Option<&Text>,
    file: Option<QuotedFile>,
) -> proto::data_message::Quote {
    proto::data_message::Quote {
        id: Some(id),
        author_aci: None,
        author_aci_binary: Some(Uuid::from(author).as_bytes().to_vec()),
        text: text.map(|t| t.body.clone()).filter(|t| !t.is_empty()),
        attachments: file
            .into_iter()
            .map(|file| proto::data_message::quote::QuotedAttachment {
                content_type: file.content_type,
                file_name: file.file_name,
                thumbnail: file.thumbnail,
            })
            .collect(),
        body_ranges: text.map(body_ranges).unwrap_or_default(),
        r#type: Some(proto::data_message::quote::Type::Normal.into()),
    }
}

/// A `Content` carrying only our sender-key distribution message (`sendSenderKeyDistributionMessage`).
pub(crate) fn distribution_content(message: Vec<u8>) -> proto::Content {
    proto::Content {
        sender_key_distribution_message: Some(message),
        ..Default::default()
    }
}

/// Kourier's ranges (UTF-8) as Signal `BodyRange`s (UTF-16 code units). Ranges Signal has no
/// form for (links, mention-all) and ranges that do not map onto the body are left out.
pub(crate) fn body_ranges(text: &Text) -> Vec<proto::BodyRange> {
    let map = Utf16Map::new(&text.body);
    text.ranges
        .iter()
        .filter_map(|range| {
            let value = match &range.kind {
                RangeKind::Bold => AssociatedValue::Style(Style::Bold.into()),
                RangeKind::Italic => AssociatedValue::Style(Style::Italic.into()),
                RangeKind::Strike => AssociatedValue::Style(Style::Strikethrough.into()),
                RangeKind::Spoiler => AssociatedValue::Style(Style::Spoiler.into()),
                RangeKind::Mono => AssociatedValue::Style(Style::Monospace.into()),
                RangeKind::Mention(user) => match crate::service_id::parse_user_id(user) {
                    Ok(crate::service_id::SignalUser::Aci(aci)) => {
                        AssociatedValue::MentionAciBinary(Uuid::from(aci).as_bytes().to_vec())
                    }
                    _ => return None,
                },
                RangeKind::MentionAll | RangeKind::Link(_) => return None,
            };
            let (start, length) = map.range_to_utf16(range.start, range.len)?;
            Some(proto::BodyRange {
                start: Some(start),
                length: Some(length),
                associated_value: Some(value),
            })
        })
        .collect()
}

/// A `Content` around a `DataMessage`.
pub(crate) fn data_content(message: proto::DataMessage) -> proto::Content {
    proto::Content {
        content: Some(proto::content::Content::DataMessage(message)),
        ..Default::default()
    }
}

/// Adds `pniSignatureMessage` (our PNI identity key signs our ACI identity key) for a contact
/// who knows us by number only, so they can merge the number into our ACI (§6.7).
pub(crate) fn add_pni_signature(content: &mut proto::Content, account: &Account) -> Result<()> {
    let signature = account
        .pni_identity
        .sign_alternate_identity(account.aci_identity.identity_key(), &mut rand::rng())?;
    content.pni_signature_message = Some(proto::PniSignatureMessage {
        pni: Some(Uuid::from(account.pni).as_bytes().to_vec()),
        signature: Some(signature.into_vec()),
    });
    Ok(())
}

/// How one recipient of a message got it, for the sync transcript.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RecipientStatus {
    /// The recipient.
    pub(crate) recipient: ServiceId,
    /// Sent sealed.
    pub(crate) sealed: bool,
}

/// What a `Transmit` put on the wire: a `DataMessage`, or an edit.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Wire {
    /// A message, a reaction, a delete or a timer change.
    Data(proto::DataMessage),
    /// A new revision of a message.
    Edit(proto::EditMessage),
}

impl Wire {
    /// The `Content` for the recipients.
    pub(crate) fn content(&self) -> proto::Content {
        match self {
            Self::Data(message) => data_content(message.clone()),
            Self::Edit(edit) => edit_content(edit.clone()),
        }
    }
}

/// The parts of a sync transcript (`SyncMessage.Sent`).
#[derive(Clone, Debug)]
pub(crate) struct Transcript<'a> {
    /// Exactly what was sent.
    pub(crate) sent: &'a Wire,
    /// Its timestamp.
    pub(crate) timestamp: u64,
    /// The 1:1 recipient (our own ACI for Note to Self).
    pub(crate) destination: Option<ServiceId>,
    /// How each recipient got it.
    pub(crate) statuses: &'a [RecipientStatus],
    /// When the disappearing timer started (the send), for messages with a timer.
    pub(crate) expiration_start: Option<u64>,
    /// A follow-up send to more recipients of an already synced message.
    pub(crate) is_recipient_update: bool,
}

/// `SyncMessage.Sent` for our other devices (`SendMessage.preload.ts:1497-1652`).
pub(crate) fn sent_transcript(transcript: &Transcript<'_>) -> proto::Content {
    let unidentified_status = transcript
        .statuses
        .iter()
        .map(
            |status| proto::sync_message::sent::UnidentifiedDeliveryStatus {
                destination_service_id_binary: Some(status.recipient.service_id_binary()),
                unidentified: Some(status.sealed),
                ..Default::default()
            },
        )
        .collect();
    sync_content(proto::SyncMessage {
        content: Some(proto::sync_message::Content::Sent(
            proto::sync_message::Sent {
                destination_service_id_binary: transcript
                    .destination
                    .map(|d| d.service_id_binary()),
                timestamp: Some(transcript.timestamp),
                message: match transcript.sent {
                    Wire::Data(message) => Some(message.clone()),
                    Wire::Edit(_) => None,
                },
                edit_message: match transcript.sent {
                    Wire::Data(_) => None,
                    Wire::Edit(edit) => Some(edit.clone()),
                },
                expiration_start_timestamp: transcript.expiration_start,
                unidentified_status,
                is_recipient_update: Some(transcript.is_recipient_update),
                ..Default::default()
            },
        )),
        ..Default::default()
    })
}

/// `SyncMessage.read` entries: messages of `sender` sent at `timestamp` that we read.
pub(crate) fn read_sync(entries: &[(Aci, u64)]) -> proto::Content {
    sync_content(proto::SyncMessage {
        read: entries
            .iter()
            .map(|(sender, timestamp)| proto::sync_message::Read {
                sender_aci_binary: Some(Uuid::from(*sender).as_bytes().to_vec()),
                timestamp: Some(*timestamp),
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    })
}

/// A conversation as sync messages name it (`ConversationIdentifier`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Thread {
    /// The 1:1 chat with this user (our own ACI: Note to Self).
    Direct(ServiceId),
    /// The group with this id.
    Group(Vec<u8>),
}

/// `SyncMessage.deleteForMe` with the messages of one conversation we deleted on this device
/// (`getDeleteForMeSyncMessage`): each by its author and sent timestamp.
pub(crate) fn delete_for_me(thread: &Thread, messages: &[(ServiceId, u64)]) -> proto::Content {
    use proto::addressable_message::Author;
    use proto::conversation_identifier::Identifier;
    let conversation = proto::ConversationIdentifier {
        identifier: Some(match thread {
            Thread::Direct(peer) => Identifier::ThreadServiceIdBinary(peer.service_id_binary()),
            Thread::Group(id) => Identifier::ThreadGroupId(id.clone()),
        }),
    };
    let messages = messages
        .iter()
        .map(|(author, timestamp)| proto::AddressableMessage {
            author: Some(Author::AuthorServiceIdBinary(author.service_id_binary())),
            sent_timestamp: Some(*timestamp),
        })
        .collect();
    sync_content(proto::SyncMessage {
        content: Some(proto::sync_message::Content::DeleteForMe(
            proto::sync_message::DeleteForMe {
                message_deletes: vec![proto::sync_message::delete_for_me::MessageDeletes {
                    conversation: Some(conversation),
                    messages,
                }],
                ..Default::default()
            },
        )),
        ..Default::default()
    })
}

/// A sync message with Desktop's random padding.
pub(crate) fn sync_content(mut sync: proto::SyncMessage) -> proto::Content {
    sync.padding = Some(random_padding());
    proto::Content {
        content: Some(proto::content::Content::SyncMessage(sync)),
        ..Default::default()
    }
}

/// What a receipt says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReceiptType {
    /// The message reached a device.
    Delivery,
    /// The user read it.
    Read,
    /// The user viewed it (view-once, voice notes).
    Viewed,
}

impl ReceiptType {
    fn proto(self) -> proto::receipt_message::Type {
        match self {
            Self::Delivery => proto::receipt_message::Type::Delivery,
            Self::Read => proto::receipt_message::Type::Read,
            Self::Viewed => proto::receipt_message::Type::Viewed,
        }
    }
}

/// A `ReceiptMessage` for the messages sent at `timestamps`.
pub(crate) fn receipt(kind: ReceiptType, timestamps: &[u64]) -> proto::Content {
    proto::Content {
        content: Some(proto::content::Content::ReceiptMessage(
            proto::ReceiptMessage {
                r#type: Some(kind.proto().into()),
                timestamp: timestamps.to_vec(),
            },
        )),
        ..Default::default()
    }
}

/// A 1:1 `TypingMessage` (STARTED or STOPPED) at `timestamp`.
pub(crate) fn typing(on: bool, timestamp: u64) -> proto::Content {
    typing_in(on, timestamp, None)
}

/// A typing indicator, in a group when `group_id` is given (`TypingMessage.groupId`).
pub(crate) fn typing_in(on: bool, timestamp: u64, group_id: Option<&[u8]>) -> proto::Content {
    let action = if on {
        proto::typing_message::Action::Started
    } else {
        proto::typing_message::Action::Stopped
    };
    proto::Content {
        content: Some(proto::content::Content::TypingMessage(
            proto::TypingMessage {
                timestamp: Some(timestamp),
                action: Some(action.into()),
                group_id: group_id.map(<[u8]>::to_vec),
            },
        )),
        ..Default::default()
    }
}

/// 1-512 random bytes, Desktop's `getRandomPadding` for sync messages.
pub(crate) fn random_padding() -> Vec<u8> {
    let mut rng = rand::rng();
    let len = rng.random_range(1..=512);
    let mut padding = vec![0; len];
    rng.fill(padding.as_mut_slice());
    padding
}

/// The sent timestamp of a sync transcript's `Content` (the envelope timestamp it goes with).
pub(crate) fn transcript_timestamp(content: &proto::Content) -> Result<u64> {
    match &content.content {
        Some(proto::content::Content::SyncMessage(proto::SyncMessage {
            content: Some(proto::sync_message::Content::Sent(sent)),
            ..
        })) => sent
            .timestamp
            .ok_or(Error::Decode("sync transcript timestamp")),
        _ => Err(Error::Decode("sync transcript")),
    }
}

#[cfg(test)]
mod tests {
    use kourier_model::{TextRange, UserId};
    use prost::Message as _;

    use super::*;
    use crate::padding::{pad, unpad};

    fn range(start: u32, len: u32, kind: RangeKind) -> TextRange {
        TextRange { start, len, kind }
    }

    #[test]
    fn body_ranges_are_utf16_and_skip_what_signal_cannot_say() {
        let mention = UserId::parse("aci:9d0652a3-dcc3-4d11-975f-74d61598733f").unwrap();
        // "é" is 2 UTF-8 bytes and 1 UTF-16 unit; "😀" is 4 and 2.
        let mut text = Text::plain("é😀 bold \u{fffc} link");
        text.ranges = vec![
            range(7, 4, RangeKind::Bold),
            range(12, 3, RangeKind::Mention(mention)),
            range(16, 4, RangeKind::Link("https://example.org".to_owned())),
            range(0, 2, RangeKind::Strike),
            range(1, 1, RangeKind::Italic), // inside "é": does not map
        ];
        let ranges = body_ranges(&text);
        assert_eq!(ranges.len(), 3, "{ranges:?}");
        assert_eq!((ranges[0].start, ranges[0].length), (Some(4), Some(4)));
        assert_eq!(
            ranges[0].associated_value,
            Some(AssociatedValue::Style(Style::Bold.into()))
        );
        assert_eq!((ranges[1].start, ranges[1].length), (Some(9), Some(1)));
        assert_eq!(
            ranges[1].associated_value,
            Some(AssociatedValue::MentionAciBinary(
                Uuid::parse_str("9d0652a3-dcc3-4d11-975f-74d61598733f")
                    .unwrap()
                    .as_bytes()
                    .to_vec()
            ))
        );
        assert_eq!((ranges[2].start, ranges[2].length), (Some(0), Some(1)));
    }

    #[test]
    fn text_messages_carry_timer_version_profile_key_and_protocol_version() {
        let plain = PreparedBody::text(&Text::plain("hello"));
        let message = text_message(&TextMessage {
            body: &plain,
            timestamp: 42,
            profile_key: &[7; 32],
            expire_timer: 0,
            expire_timer_version: 0,
            quote: None,
        });
        assert_eq!(message.body.as_deref(), Some("hello"));
        assert_eq!(message.timestamp, Some(42));
        assert_eq!(message.profile_key.as_deref(), Some(&[7u8; 32][..]));
        assert_eq!(message.expire_timer, Some(0), "always sent in 1:1 chats");
        assert_eq!(
            message.expire_timer_version,
            Some(1),
            "Desktop's first version"
        );
        assert_eq!(message.required_protocol_version, None);

        let mut bold = Text::plain("hello");
        bold.ranges = vec![range(0, 5, RangeKind::Bold)];
        let message = text_message(&TextMessage {
            body: &PreparedBody::text(&bold),
            timestamp: 42,
            profile_key: &[7; 32],
            expire_timer: 3600,
            expire_timer_version: 4,
            quote: None,
        });
        assert_eq!(message.required_protocol_version, Some(6));
        assert_eq!(message.expire_timer, Some(3600));
        assert_eq!(message.expire_timer_version, Some(4));
    }

    #[test]
    fn contents_round_trip_padded_and_carry_a_verifiable_pni_signature() {
        let account = crate::db::test_account();
        let plain = PreparedBody::text(&Text::plain("padded"));
        let mut content = data_content(text_message(&TextMessage {
            body: &plain,
            timestamp: 1,
            profile_key: &account.profile_key,
            expire_timer: 0,
            expire_timer_version: 1,
            quote: None,
        }));
        add_pni_signature(&mut content, &account).unwrap();
        let bytes = content.encode_to_vec();
        let padded = pad(&bytes);
        assert_eq!(padded.len() % 80, 79, "one byte short of a block multiple");
        let decoded = proto::Content::decode(unpad(&padded).unwrap()).unwrap();
        assert_eq!(decoded, content);
        let signature = decoded.pni_signature_message.unwrap();
        assert_eq!(
            signature.pni.as_deref(),
            Some(Uuid::from(account.pni).as_bytes().as_slice())
        );
        assert!(
            account
                .pni_identity
                .identity_key()
                .verify_alternate_identity(
                    account.aci_identity.identity_key(),
                    signature.signature.as_deref().unwrap()
                )
                .unwrap()
        );
    }

    #[test]
    fn transcripts_receipts_typing_and_read_syncs() {
        let peer = ServiceId::from(Aci::from(Uuid::from_u128(5)));
        let message = proto::DataMessage {
            body: Some("x".to_owned()),
            timestamp: Some(9),
            ..Default::default()
        };
        let wire = Wire::Data(message.clone());
        let content = sent_transcript(&Transcript {
            sent: &wire,
            timestamp: 9,
            destination: Some(peer),
            statuses: &[RecipientStatus {
                recipient: peer,
                sealed: true,
            }],
            expiration_start: None,
            is_recipient_update: false,
        });
        assert_eq!(transcript_timestamp(&content).unwrap(), 9);
        let Some(proto::content::Content::SyncMessage(sync)) = &content.content else {
            panic!("sync");
        };
        let padding = sync.padding.as_ref().unwrap().len();
        assert!((1..=512).contains(&padding));
        let Some(proto::sync_message::Content::Sent(sent)) = &sync.content else {
            panic!("sent");
        };
        assert_eq!(sent.message.as_ref(), Some(&message));
        assert_eq!(
            sent.destination_service_id_binary,
            Some(peer.service_id_binary())
        );
        assert_eq!(sent.unidentified_status[0].unidentified, Some(true));
        assert_eq!(sent.is_recipient_update, Some(false));
        assert!(transcript_timestamp(&receipt(ReceiptType::Read, &[1])).is_err());

        let Some(proto::content::Content::ReceiptMessage(r)) =
            receipt(ReceiptType::Delivery, &[1, 2]).content
        else {
            panic!("receipt");
        };
        assert_eq!(r.r#type(), proto::receipt_message::Type::Delivery);
        assert_eq!(r.timestamp, [1, 2]);
        let Some(proto::content::Content::TypingMessage(t)) = typing(false, 3).content else {
            panic!("typing");
        };
        assert_eq!(t.action(), proto::typing_message::Action::Stopped);
        assert_eq!(t.timestamp, Some(3));
        let aci = Aci::from(Uuid::from_u128(6));
        let Some(proto::content::Content::SyncMessage(s)) = read_sync(&[(aci, 4)]).content else {
            panic!("read sync");
        };
        assert_eq!(
            s.read[0].sender_aci_binary.as_deref(),
            Some(Uuid::from(aci).as_bytes().as_slice())
        );
        assert_eq!(s.read[0].timestamp, Some(4));
    }

    #[test]
    fn group_texts_carry_the_group_and_its_timer_but_no_timer_version() {
        let text = PreparedBody::text(&Text::plain("hello group"));
        let message = in_group(
            text_message(&TextMessage {
                body: &text,
                timestamp: 9,
                profile_key: &[7; 32],
                expire_timer: 600,
                expire_timer_version: 0,
                quote: None,
            }),
            GroupContext {
                master_key: &[3; 32],
                revision: 12,
            },
        );
        assert_eq!(message.body.as_deref(), Some("hello group"));
        assert_eq!(message.expire_timer, Some(600));
        assert_eq!(message.expire_timer_version, None, "1:1 chats only");
        let group = message.group_v2.expect("group context");
        assert_eq!(group.master_key.as_deref(), Some(&[3u8; 32][..]));
        assert_eq!(group.revision, Some(12));
        let typing = typing_in(true, 5, Some(&[1, 2]));
        let Some(proto::content::Content::TypingMessage(typing)) = typing.content else {
            panic!("typing");
        };
        assert_eq!(typing.group_id.as_deref(), Some(&[1u8, 2][..]));
        let distribution = distribution_content(vec![8, 9]);
        assert_eq!(
            distribution.sender_key_distribution_message.as_deref(),
            Some(&[8u8, 9][..])
        );
        assert!(distribution.content.is_none());
    }

    fn aci(n: u128) -> Aci {
        Aci::from(Uuid::from_u128(n))
    }

    /// `sendReaction.preload.ts`: emoji, remove, the target's author (binary) and timestamp,
    /// the chat's timer with its version, our profile key; in a group the context instead of
    /// the version.
    #[test]
    fn reactions_name_their_target_and_keep_the_chats_timer() {
        let reaction = Reaction {
            emoji: "👍",
            remove: false,
            target_author: aci(7),
            target_timestamp: 1000,
        };
        let message = reaction_message(&reaction, 2000, &[9; 32], 3600, 5);
        let sent = message.reaction.as_ref().expect("reaction");
        assert_eq!(sent.emoji.as_deref(), Some("👍"));
        assert_eq!(sent.remove, Some(false));
        assert_eq!(sent.target_sent_timestamp, Some(1000));
        assert_eq!(
            sent.target_author_aci_binary.as_deref(),
            Some(Uuid::from_u128(7).as_bytes().as_slice())
        );
        assert_eq!(sent.target_author_aci, None, "binary ids only");
        assert_eq!(message.timestamp, Some(2000));
        assert_eq!(message.profile_key.as_deref(), Some(&[9u8; 32][..]));
        assert_eq!(
            (message.expire_timer, message.expire_timer_version),
            (Some(3600), Some(5))
        );
        assert!(message.body.is_none() && message.attachments.is_empty());
        assert_eq!(message.required_protocol_version, None);

        // Taking it back names the emoji that goes (Desktop's `emojiToRemove`).
        let removal = reaction_message(
            &Reaction {
                remove: true,
                ..reaction
            },
            2001,
            &[9; 32],
            0,
            0,
        );
        let sent = removal.reaction.expect("reaction");
        assert_eq!(
            (sent.emoji.as_deref(), sent.remove),
            (Some("👍"), Some(true))
        );
        assert_eq!(
            removal.expire_timer_version,
            Some(1),
            "never below Desktop's first version"
        );

        let grouped = in_group(
            message,
            GroupContext {
                master_key: &[3; 32],
                revision: 4,
            },
        );
        assert_eq!(grouped.expire_timer_version, None);
        assert_eq!(grouped.expire_timer, Some(3600));
        assert_eq!(grouped.group_v2.and_then(|g| g.revision), Some(4));
    }

    /// `sendDeleteForEveryone.preload.ts`: the target's timestamp and our profile key, nothing
    /// about timers; an admin's delete names the author too.
    #[test]
    fn deletes_carry_only_their_target() {
        let message = delete_message(1000, 2000, &[9; 32]);
        assert_eq!(
            message
                .delete
                .as_ref()
                .and_then(|d| d.target_sent_timestamp),
            Some(1000)
        );
        assert_eq!(message.timestamp, Some(2000));
        assert_eq!(message.profile_key.as_deref(), Some(&[9u8; 32][..]));
        assert_eq!(
            (message.expire_timer, message.expire_timer_version),
            (None, None)
        );
        assert!(message.admin_delete.is_none());

        let admin = admin_delete_message(aci(7), 1000, 2000, &[9; 32]);
        assert!(admin.delete.is_none());
        let sent = admin.admin_delete.expect("admin delete");
        assert_eq!(sent.target_sent_timestamp, Some(1000));
        assert_eq!(
            sent.target_author_aci_binary.as_deref(),
            Some(Uuid::from_u128(7).as_bytes().as_slice())
        );
    }

    /// `getDataOrEditMessage`: an edit is a whole message with its own timestamp inside an
    /// `EditMessage` naming the revision it replaces; its transcript carries the edit, not a
    /// message.
    #[test]
    fn edits_wrap_a_whole_message_and_their_transcript_carries_the_edit() {
        let body = PreparedBody::text(&Text::plain("fixed"));
        let message = text_message(&TextMessage {
            body: &body,
            timestamp: 3000,
            profile_key: &[9; 32],
            expire_timer: 60,
            expire_timer_version: 2,
            quote: None,
        });
        let edit = edit_message(1000, message.clone());
        assert_eq!(edit.target_sent_timestamp, Some(1000));
        assert_eq!(edit.data_message.as_ref(), Some(&message));
        let wire = Wire::Edit(edit.clone());
        let Some(proto::content::Content::EditMessage(sent)) = wire.content().content else {
            panic!("an edit content");
        };
        assert_eq!(sent, edit);
        let peer = ServiceId::from(aci(5));
        let transcript = sent_transcript(&Transcript {
            sent: &wire,
            timestamp: 3000,
            destination: Some(peer),
            statuses: &[],
            expiration_start: None,
            is_recipient_update: false,
        });
        assert_eq!(transcript_timestamp(&transcript).unwrap(), 3000);
        let Some(proto::content::Content::SyncMessage(sync)) = transcript.content else {
            panic!("sync");
        };
        let Some(proto::sync_message::Content::Sent(sent)) = sync.content else {
            panic!("sent");
        };
        assert_eq!(sent.edit_message, Some(edit));
        assert_eq!(sent.message, None, "an edit transcript has no message");
        assert_eq!(sent.timestamp, Some(3000), "the edit's own timestamp");
    }

    /// `Message.toProto`'s quote: id, author (binary), text with UTF-16 ranges, the quoted
    /// attachment with its thumbnail, type NORMAL; ranges in a quote need protocol version 6.
    #[test]
    fn quotes_carry_author_text_ranges_and_the_attachment_stub() {
        let mut quoted = Text::plain("😀 bold");
        quoted.ranges = vec![range(5, 4, RangeKind::Bold)];
        let thumbnail = proto::AttachmentPointer {
            content_type: Some("image/jpeg".to_owned()),
            size: Some(10),
            ..Default::default()
        };
        let sent = quote(
            1000,
            aci(7),
            Some(&quoted),
            Some(QuotedFile {
                content_type: Some("image/png".to_owned()),
                file_name: Some("cat.png".to_owned()),
                thumbnail: Some(thumbnail.clone()),
            }),
        );
        assert_eq!(sent.id, Some(1000));
        assert_eq!(
            sent.author_aci_binary.as_deref(),
            Some(Uuid::from_u128(7).as_bytes().as_slice())
        );
        assert_eq!(sent.author_aci, None);
        assert_eq!(sent.text.as_deref(), Some("😀 bold"));
        assert_eq!(sent.r#type, Some(0), "NORMAL");
        assert_eq!(
            (sent.body_ranges[0].start, sent.body_ranges[0].length),
            (Some(3), Some(4)),
            "UTF-16 offsets"
        );
        assert_eq!(sent.attachments.len(), 1);
        assert_eq!(
            sent.attachments[0].content_type.as_deref(),
            Some("image/png")
        );
        assert_eq!(sent.attachments[0].file_name.as_deref(), Some("cat.png"));
        assert_eq!(sent.attachments[0].thumbnail, Some(thumbnail));

        let reply = PreparedBody::text(&Text::plain("yes"));
        let message = text_message(&TextMessage {
            body: &reply,
            timestamp: 2000,
            profile_key: &[9; 32],
            expire_timer: 0,
            expire_timer_version: 1,
            quote: Some(&sent),
        });
        assert_eq!(message.quote.as_ref(), Some(&sent));
        assert_eq!(message.required_protocol_version, Some(6));

        // A quote of a picture without text: no text field, no ranges, no version bump.
        let bare = quote(1000, aci(7), None, None);
        assert_eq!((bare.text, bare.attachments.len()), (None, 0));
        let message = text_message(&TextMessage {
            body: &reply,
            timestamp: 2000,
            profile_key: &[9; 32],
            expire_timer: 0,
            expire_timer_version: 1,
            quote: Some(&quote(1000, aci(7), None, None)),
        });
        assert_eq!(message.required_protocol_version, None);
    }

    /// `sendDirectExpirationTimerUpdate.preload.ts`: flag 2, the timer, its version, the
    /// profile key, nothing else.
    #[test]
    fn timer_updates_carry_the_flag_the_timer_and_its_version() {
        let message = timer_update_message(5000, &[9; 32], 30, 3);
        assert_eq!(message.flags, Some(2));
        assert_eq!(
            (message.expire_timer, message.expire_timer_version),
            (Some(30), Some(3))
        );
        assert_eq!(message.timestamp, Some(5000));
        assert_eq!(message.profile_key.as_deref(), Some(&[9u8; 32][..]));
        assert!(message.body.is_none());
        let off = timer_update_message(5001, &[9; 32], 0, 4);
        assert_eq!(off.expire_timer, Some(0), "off is sent as 0");
    }

    /// `getDeleteForMeSyncMessage`: one `MessageDeletes` per conversation, each message by
    /// author (binary service id) and sent timestamp.
    #[test]
    fn delete_for_me_names_the_conversation_and_each_message() {
        use proto::addressable_message::Author;
        use proto::conversation_identifier::Identifier;
        let peer = ServiceId::from(aci(5));
        let me = ServiceId::from(aci(1));
        let deletes = |content: proto::Content| {
            let Some(proto::content::Content::SyncMessage(sync)) = content.content else {
                panic!("sync");
            };
            assert!(sync.padding.is_some());
            let Some(proto::sync_message::Content::DeleteForMe(delete)) = sync.content else {
                panic!("delete for me");
            };
            assert!(delete.conversation_deletes.is_empty() && delete.attachment_deletes.is_empty());
            delete.message_deletes
        };
        let direct = deletes(delete_for_me(
            &Thread::Direct(peer),
            &[(peer, 10), (me, 11)],
        ));
        assert_eq!(direct.len(), 1);
        assert_eq!(
            direct[0]
                .conversation
                .as_ref()
                .and_then(|c| c.identifier.clone()),
            Some(Identifier::ThreadServiceIdBinary(peer.service_id_binary()))
        );
        assert_eq!(direct[0].messages.len(), 2);
        assert_eq!(
            direct[0].messages[1].author,
            Some(Author::AuthorServiceIdBinary(me.service_id_binary()))
        );
        assert_eq!(direct[0].messages[1].sent_timestamp, Some(11));
        let group = deletes(delete_for_me(&Thread::Group(vec![4; 32]), &[(peer, 12)]));
        assert_eq!(
            group[0]
                .conversation
                .as_ref()
                .and_then(|c| c.identifier.clone()),
            Some(Identifier::ThreadGroupId(vec![4; 32]))
        );
    }
}
