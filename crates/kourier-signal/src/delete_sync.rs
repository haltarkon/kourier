//! Deletes for me made on this device, told to our other devices (§3.5.2, §6.13;
//! `SD/ts/textsecure/SendMessage.preload.ts` `getDeleteForMeSyncMessage`,
//! `SD/ts/models/conversations.preload.ts` `destroyMessages`, `SD/ts/util/syncIdentifiers.preload.ts`).
//!
//! The core has deleted locally already and hands over a durable command; one zone stores the
//! `SyncMessage.deleteForMe` as a job (keyed by the command and a digest of its work, so a
//! command the core sends again finds its job) and confirms with `CommandPersisted`. The job
//! sends it to our own devices like every sync message: unsealed, not urgent
//! (`urgent: false` in `getDeleteForMeSyncMessage`).
//!
//! - `Command::ClearConversation` becomes a `ConversationDelete`: the conversation, the newest
//!   messages as `mostRecentMessages` (the other devices delete up to the newest of them they
//!   have), and `mostRecentNonExpiringMessages` only when it says something else (Desktop
//!   sends it only when one of the newest messages disappears). `isFullDelete` is the
//!   command's `remove`: the chat itself was deleted, not only emptied. A conversation that
//!   held no message a device could be told about and was deleted becomes a
//!   `LocalOnlyConversationDelete`, as in Desktop.
//!
//!   **`isFullDelete: false` empties the chat and keeps it, on all three official clients**
//!   (read from their receive code, 2026-10-07; `Android:` and `iOS:` name files of the
//!   public repositories), so "clear chat" sends false and "delete chat" true:
//!   - Desktop removes the messages up to the newest anchor it has and goes on to hide the
//!     conversation only for a full delete (`SD/ts/util/deleteForMe.preload.ts`
//!     `deleteConversation`: `if (isFullDelete) … deleteLocalOnlyConversation`, which calls
//!     `destroyMessagesInner` and so takes the conversation out of the list). Desktop itself
//!     always sends true (`SD/ts/textsecure/SendMessage.preload.ts`
//!     `getDeleteForMeSyncMessage`), because its only such action is "delete chat".
//!   - Android trims the thread up to the anchor and deletes the thread only for a full
//!     delete (Android: `messages/SyncMessageProcessor.kt`
//!     `handleSynchronizeConversationDeletes`: `threads.trimThread(…, trimBeforeDate =
//!     latestReceivedAt, inclusive = true)`, then `if (delete.isFullDelete == true)
//!     deleteConversationIfContainsOnlyLocal`). Android sends false itself, for its message
//!     trims (`database/ThreadTable.kt` `trimThread` →
//!     `MultiDeviceDeleteSyncJob.enqueueThreadDeletes(…, isFullDelete = false)`), and true
//!     for a deleted chat (`deleteConversations`).
//!   - iOS deletes the interactions up to the anchor and soft-deletes the thread (which
//!     takes it out of the chat list) only for a full delete (iOS:
//!     `SignalServiceKit/Messages/DeviceSyncing/DeleteForMe/DeleteForMeIncomingSyncMessageManager.swift`
//!     `handleConversationDelete` → `SignalServiceKit/Jobs/BulkDeleteInteractionJobQueue.swift`
//!     `isFullThreadDelete`).
//!
//!   Two things every such entry needs, or a receiver drops it: the flag must be **present**
//!   on the wire even when it is false (iOS: `DeleteForMeSyncMessageReceiver.swift` skips an
//!   entry unless `conversationDelete.hasIsFullDelete`), which is why the field is always
//!   `Some`; and `mostRecentMessages` must name a message (Desktop:
//!   `SD/ts/textsecure/MessageReceiver.preload.ts` `handleDeleteForMeSync` drops an entry
//!   without target messages; Android and iOS find no anchor and do nothing), which is why
//!   an emptied chat without one sends nothing.
//! - `Command::DeleteAttachment` becomes an `AttachmentDelete`: the conversation, the message,
//!   and the attachment by the names Signal has for it, `clientUuid` and `fallbackDigest`
//!   (`Attachment::net_ids`, [`crate::receive::media`]). Without one of these two names no
//!   other device could find the attachment: nothing is sent.
//! - Addressing follows Desktop: a group by its id (`threadGroupId`), a 1:1 chat by the
//!   peer's service id in binary form (`threadServiceIdBinary`; Note to Self is our own ACI), a
//!   message by its author's service id in binary form and its sent timestamp. Desktop names
//!   a contact without an ACI by phone number; Kourier knows such a chat by the PNI and names
//!   that, which Desktop reads (`MessageReceiver.preload.ts` `processConversationIdentifier`
//!   takes a PNI there; the phones are unverified). An author Kourier knows only by number is
//!   named by number (`authorE164`). Messages without a Signal id (notices) are left out.

use std::rc::Rc;
use std::sync::Arc;

use kourier_db::Tx;
use kourier_model::{CmdId, ConvKey, MsgTarget, TargetAuthor};
use libsignal_core::ServiceId;
use prost::Message as _;
use tokio::sync::Notify;

use crate::actor::Shared;
use crate::db::Account;
use crate::error::{Error, Result};
use crate::groups;
use crate::jobs::{self, Done, Job, JobKind};
use crate::net::SignalTransport;
use crate::outbox::append_events;
use crate::proto;
use crate::proto::sync_message::DeleteForMe;
use crate::proto::sync_message::delete_for_me::{
    AttachmentDelete, ConversationDelete, LocalOnlyConversationDelete,
};
use crate::send::content;
use crate::send::{self, Delivery, Outgoing};
use crate::service_id::{SignalUser, parse_user_id};

/// The conversation `conv` as sync messages name it; `None` for a key that is not a Signal
/// conversation (a chat known only by phone number has no thread other devices could find).
pub(crate) fn conversation(conv: &ConvKey) -> Option<proto::ConversationIdentifier> {
    use proto::conversation_identifier::Identifier;
    let identifier = match conv {
        ConvKey::Direct(user) => match parse_user_id(user).ok()? {
            SignalUser::Aci(aci) => {
                Identifier::ThreadServiceIdBinary(ServiceId::from(aci).service_id_binary())
            }
            SignalUser::Pni(pni) => {
                Identifier::ThreadServiceIdBinary(ServiceId::from(pni).service_id_binary())
            }
            SignalUser::E164(number) => Identifier::ThreadE164(number.to_string()),
        },
        ConvKey::Group(_) => Identifier::ThreadGroupId(groups::id_of_conv(conv)?),
        // Note to Self arrives as the direct chat with our own ACI; any other key is not
        // Signal's.
        _ => return None,
    };
    Some(proto::ConversationIdentifier {
        identifier: Some(identifier),
    })
}

/// The message `target` as sync messages name it: author and sent timestamp. `None` for a
/// target without a Signal id (a notice) or without a nameable author.
pub(crate) fn addressable(
    account: &Account,
    target: &MsgTarget,
) -> Option<proto::AddressableMessage> {
    use proto::addressable_message::Author;
    let sent_timestamp: u64 = target.local.parse().ok()?;
    let author = match &target.author {
        TargetAuthor::Own => {
            Author::AuthorServiceIdBinary(ServiceId::from(account.aci).service_id_binary())
        }
        TargetAuthor::User(user) => match parse_user_id(user).ok()? {
            SignalUser::Aci(aci) => {
                Author::AuthorServiceIdBinary(ServiceId::from(aci).service_id_binary())
            }
            SignalUser::Pni(pni) => {
                Author::AuthorServiceIdBinary(ServiceId::from(pni).service_id_binary())
            }
            SignalUser::E164(number) => Author::AuthorE164(number.to_string()),
        },
        TargetAuthor::Unknown => return None,
    };
    Some(proto::AddressableMessage {
        author: Some(author),
        sent_timestamp: Some(sent_timestamp),
    })
}

/// The `deleteForMe` of a cleared or deleted conversation; `None` when no other device could
/// be told anything (an unknown key, or an emptied chat without a message to delete up to).
fn clear_message(
    account: &Account,
    conv: &ConvKey,
    newest: &[MsgTarget],
    newest_non_expiring: &[MsgTarget],
    remove: bool,
) -> Option<DeleteForMe> {
    let conversation = conversation(conv)?;
    let address = |targets: &[MsgTarget]| -> Vec<proto::AddressableMessage> {
        targets
            .iter()
            .filter_map(|target| addressable(account, target))
            .take(kourier_backend_api::CLEAR_ANCHORS)
            .collect()
    };
    let most_recent = address(newest);
    if most_recent.is_empty() {
        // Nothing another device could delete up to. A deleted chat is still told (Desktop's
        // `delete-local-conversation`); an emptied one has nothing to say.
        return remove.then(|| DeleteForMe {
            local_only_conversation_deletes: vec![LocalOnlyConversationDelete {
                conversation: Some(conversation),
            }],
            ..Default::default()
        });
    }
    let non_expiring = address(newest_non_expiring);
    Some(DeleteForMe {
        conversation_deletes: vec![ConversationDelete {
            conversation: Some(conversation),
            // Desktop names them only when one of the newest messages disappears.
            most_recent_non_expiring_messages: if non_expiring == most_recent {
                Vec::new()
            } else {
                non_expiring
            },
            most_recent_messages: most_recent,
            // Always present, also when false: iOS drops an entry without the flag, and
            // false is what keeps the chat on every client (module docs).
            is_full_delete: Some(remove),
        }],
        ..Default::default()
    })
}

/// The `deleteForMe` of one deleted attachment; `None` when it has no name another device
/// knows it by (or the message or the conversation cannot be named).
fn attachment_message(
    account: &Account,
    conv: &ConvKey,
    target: &MsgTarget,
    net_ids: &[String],
) -> Option<DeleteForMe> {
    let client_uuid = crate::receive::media::uuid_of_net_ids(net_ids);
    let fallback_digest = crate::receive::media::digest_of_net_ids(net_ids);
    // An imported attachment of a file the phone had downloaded has a plaintext hash and
    // often no digest; Android sends and reads this name too.
    let fallback_plaintext_hash = crate::receive::media::plaintext_hash_of_net_ids(net_ids);
    if client_uuid.is_none() && fallback_digest.is_none() && fallback_plaintext_hash.is_none() {
        return None;
    }
    Some(DeleteForMe {
        attachment_deletes: vec![AttachmentDelete {
            conversation: Some(conversation(conv)?),
            target_message: Some(addressable(account, target)?),
            client_uuid,
            fallback_digest,
            fallback_plaintext_hash,
        }],
        ..Default::default()
    })
}

/// Stores the job of `message` (if there is something to tell) and confirms `cmd`, in the
/// caller's zone. Returns whether a job was added.
fn persist(
    tx: &mut Tx<'_>,
    cmd: CmdId,
    message: Option<DeleteForMe>,
    now: i64,
    wake: &Rc<Notify>,
) -> Result<bool> {
    let added = match message {
        // The job's payload is the sync message without its random padding, so the same
        // command always has the same digest (`jobs::enqueue_command`).
        Some(message) => jobs::enqueue_command(
            tx,
            JobKind::DeleteSync,
            cmd.0,
            &message.encode_to_vec(),
            now,
        )?,
        None => false,
    };
    append_events(
        tx,
        &[kourier_backend_api::BackendEvent::CommandPersisted { cmd }],
        now,
        wake,
    )?;
    Ok(added)
}

/// `Command::ClearConversation`: the job (when another device can be told something) and
/// `CommandPersisted` in one zone. Returns whether a job was added.
#[allow(
    clippy::too_many_arguments,
    reason = "the command's fields plus the zone's context"
)]
pub(crate) fn persist_clear(
    tx: &mut Tx<'_>,
    account: &Account,
    cmd: CmdId,
    conv: &ConvKey,
    newest: &[MsgTarget],
    newest_non_expiring: &[MsgTarget],
    remove: bool,
    now: i64,
    wake: &Rc<Notify>,
) -> Result<bool> {
    let message = clear_message(account, conv, newest, newest_non_expiring, remove);
    persist(tx, cmd, message, now, wake)
}

/// `Command::DeleteAttachment`: the job (when the attachment has a name another device knows)
/// and `CommandPersisted` in one zone. Returns whether a job was added.
#[allow(
    clippy::too_many_arguments,
    reason = "the command's fields plus the zone's context"
)]
pub(crate) fn persist_attachment(
    tx: &mut Tx<'_>,
    account: &Account,
    cmd: CmdId,
    conv: &ConvKey,
    target: &MsgTarget,
    net_ids: &[String],
    now: i64,
    wake: &Rc<Notify>,
) -> Result<bool> {
    let message = attachment_message(account, conv, target, net_ids);
    persist(tx, cmd, message, now, wake)
}

/// Runs a `delete_sync` job: the stored `deleteForMe` to our other devices.
pub(crate) async fn run<T: SignalTransport>(
    shared: &Rc<Shared<T>>,
    socket: &Arc<T::Socket>,
    job: &Job,
) -> Result<Done> {
    let Some(delete) = job
        .payload
        .as_deref()
        .and_then(|p| DeleteForMe::decode(p).ok())
    else {
        tracing::warn!("delete sync job without a valid payload dropped");
        return Ok(Done::Finished);
    };
    let (conversations, local_only, attachments) = (
        delete.conversation_deletes.len(),
        delete.local_only_conversation_deletes.len(),
        delete.attachment_deletes.len(),
    );
    let sync = content::sync_content(proto::SyncMessage {
        content: Some(proto::sync_message::Content::DeleteForMe(delete)),
        ..Default::default()
    });
    let account = shared.account().ok_or(Error::NotLinked)?;
    let ctx = shared.send_context(&account, socket);
    send::send_to(
        &ctx,
        account.aci.into(),
        &Outgoing {
            content: &sync,
            timestamp: shared.unique_timestamp(),
            delivery: Delivery::TRANSCRIPT,
            local: None,
            group: None,
        },
    )
    .await?;
    tracing::debug!(
        conversations,
        local_only,
        attachments,
        "delete for me synced"
    );
    Ok(Done::Finished)
}

#[cfg(test)]
mod tests {
    use kourier_backend_api::BackendEvent;
    use kourier_model::UserId;
    use libsignal_core::{Aci, Pni};
    use uuid::Uuid;

    use super::*;
    use crate::receive::tests::fixture;
    use crate::service_id::{aci_user_id, pni_user_id};

    fn target(author: TargetAuthor, local: &str) -> MsgTarget {
        MsgTarget {
            conv: None,
            author,
            local: local.into(),
            alternates: Vec::new(),
        }
    }

    fn from(user: &UserId, local: u64) -> MsgTarget {
        target(TargetAuthor::User(user.clone()), &local.to_string())
    }

    fn thread(delete: &ConversationDelete) -> proto::conversation_identifier::Identifier {
        delete
            .conversation
            .clone()
            .and_then(|c| c.identifier)
            .expect("conversation")
    }

    fn author_and_time(message: &proto::AddressableMessage) -> (Vec<u8>, u64) {
        use proto::addressable_message::Author;
        match message.author.clone() {
            Some(Author::AuthorServiceIdBinary(bytes)) => (bytes, message.sent_timestamp.unwrap()),
            other => panic!("{other:?}"),
        }
    }

    /// A cleared chat names the conversation and its newest messages the way Desktop does;
    /// the non-expiring list is sent only when it differs; a deleted chat is a full delete.
    #[test]
    fn a_cleared_chat_names_its_newest_messages() {
        use proto::conversation_identifier::Identifier;
        let fixture = fixture();
        let account = &fixture.account;
        let alice = Aci::from(Uuid::from_u128(0xa11ce));
        let user = aci_user_id(alice).unwrap();
        let conv = ConvKey::Direct(user.clone());
        let own = ServiceId::from(account.aci).service_id_binary();
        let theirs = ServiceId::from(alice).service_id_binary();
        let newest = [
            from(&user, 50),
            target(TargetAuthor::Own, "40"),
            from(&user, 30),
        ];

        let delete = clear_message(account, &conv, &newest, &newest, false).expect("a message");
        assert!(delete.message_deletes.is_empty() && delete.attachment_deletes.is_empty());
        assert!(delete.local_only_conversation_deletes.is_empty());
        let [chat] = &delete.conversation_deletes[..] else {
            panic!("{delete:?}");
        };
        assert_eq!(
            thread(chat),
            Identifier::ThreadServiceIdBinary(theirs.clone())
        );
        assert_eq!(
            chat.most_recent_messages
                .iter()
                .map(author_and_time)
                .collect::<Vec<_>>(),
            [
                (theirs.clone(), 50),
                (own.clone(), 40),
                (theirs.clone(), 30)
            ],
            "newest first, own messages by our ACI"
        );
        assert!(
            chat.most_recent_non_expiring_messages.is_empty(),
            "none of the newest disappears: the second list says nothing new"
        );
        assert_eq!(chat.is_full_delete, Some(false), "emptied, not deleted");
        // On the wire the flag is there although it is false (field 3, varint 0): iOS drops
        // a conversation delete without it, and a proto2 reader tells "false" from "unset".
        let bytes = chat.encode_to_vec();
        assert!(
            bytes.windows(2).any(|pair| pair == [0x18, 0x00]),
            "isFullDelete = false is encoded: {bytes:02x?}"
        );
        let read = ConversationDelete::decode(&bytes[..]).unwrap();
        assert_eq!(
            read.is_full_delete,
            Some(false),
            "present after a round trip"
        );
        let unset = ConversationDelete {
            is_full_delete: None,
            ..chat.clone()
        };
        assert_eq!(
            ConversationDelete::decode(&unset.encode_to_vec()[..])
                .unwrap()
                .is_full_delete,
            None,
            "an unset flag reads as unset, which is what iOS refuses"
        );

        // One of the newest disappears: the other devices may have lost it already, so the
        // newest messages that stay are named too. The chat itself was deleted.
        let lasting = [target(TargetAuthor::Own, "40"), from(&user, 30)];
        let delete = clear_message(account, &conv, &newest, &lasting, true).expect("a message");
        let chat = &delete.conversation_deletes[0];
        assert_eq!(chat.is_full_delete, Some(true));
        assert_eq!(chat.most_recent_messages.len(), 3);
        assert_eq!(
            chat.most_recent_non_expiring_messages
                .iter()
                .map(author_and_time)
                .collect::<Vec<_>>(),
            [(own.clone(), 40), (theirs.clone(), 30)]
        );

        // A notice has no Signal id and names nothing; at most five anchors go out.
        let many: Vec<MsgTarget> = (0..8u64).map(|i| from(&user, 100 - i)).collect();
        let mut with_notice = vec![from(&user, 200), target(TargetAuthor::Own, "timer:7")];
        with_notice.extend(many);
        let delete = clear_message(account, &conv, &with_notice, &with_notice, false).unwrap();
        let sent: Vec<u64> = delete.conversation_deletes[0]
            .most_recent_messages
            .iter()
            .map(|m| m.sent_timestamp.unwrap())
            .collect();
        assert_eq!(sent, [200, 100, 99, 98, 97]);

        // A group is named by its id, Note to Self by our own ACI, a chat with a contact we
        // know only by PNI by that PNI.
        let keys = crate::groups::GroupKeys::from_master([4; 32]);
        let delete = clear_message(account, &keys.conv_key(), &newest, &newest, true).unwrap();
        assert_eq!(
            thread(&delete.conversation_deletes[0]),
            Identifier::ThreadGroupId(keys.id.to_vec())
        );
        let me = ConvKey::Direct(aci_user_id(account.aci).unwrap());
        let mine = [target(TargetAuthor::Own, "9")];
        let delete = clear_message(account, &me, &mine, &mine, false).unwrap();
        assert_eq!(
            thread(&delete.conversation_deletes[0]),
            Identifier::ThreadServiceIdBinary(own.clone())
        );
        let pni = Pni::from(Uuid::from_u128(0xb0b));
        let by_number = ConvKey::Direct(pni_user_id(pni).unwrap());
        let delete = clear_message(account, &by_number, &mine, &mine, true).unwrap();
        assert_eq!(
            thread(&delete.conversation_deletes[0]),
            Identifier::ThreadServiceIdBinary(ServiceId::from(pni).service_id_binary())
        );
    }

    /// A deleted chat without a message another device could be told about is a local-only
    /// delete (Desktop's `delete-local-conversation`); an emptied one says nothing.
    #[test]
    fn a_chat_without_anchors_is_a_local_only_delete_or_nothing() {
        let fixture = fixture();
        let account = &fixture.account;
        let alice = Aci::from(Uuid::from_u128(0xa11ce));
        let conv = ConvKey::Direct(aci_user_id(alice).unwrap());
        let delete = clear_message(account, &conv, &[], &[], true).expect("told");
        assert!(delete.conversation_deletes.is_empty());
        let [local] = &delete.local_only_conversation_deletes[..] else {
            panic!("{delete:?}");
        };
        assert_eq!(
            local.conversation.clone().and_then(|c| c.identifier),
            Some(
                proto::conversation_identifier::Identifier::ThreadServiceIdBinary(
                    ServiceId::from(alice).service_id_binary()
                )
            )
        );
        assert_eq!(clear_message(account, &conv, &[], &[], false), None);
        // Only notices were left: the same as none.
        let notices = [target(TargetAuthor::Own, "timer:5")];
        assert!(
            clear_message(account, &conv, &notices, &notices, true)
                .is_some_and(|d| d.local_only_conversation_deletes.len() == 1)
        );
        assert_eq!(
            clear_message(account, &conv, &notices, &notices, false),
            None
        );
    }

    /// A deleted attachment is named by its client UUID and its digest, as the phone names
    /// one; without either name nothing is sent.
    #[test]
    fn a_deleted_attachment_is_named_as_the_phone_names_it() {
        let fixture = fixture();
        let account = &fixture.account;
        let alice = Aci::from(Uuid::from_u128(0xa11ce));
        let user = aci_user_id(alice).unwrap();
        let conv = ConvKey::Direct(user.clone());
        let uuid = [7u8; 16];
        let digest = [9u8; 32];
        let names = vec![
            crate::receive::media::net_id_uuid(&uuid).unwrap(),
            crate::receive::media::net_id_digest(&digest).unwrap(),
        ];
        let delete =
            attachment_message(account, &conv, &from(&user, 77), &names).expect("a message");
        let [attachment] = &delete.attachment_deletes[..] else {
            panic!("{delete:?}");
        };
        assert_eq!(attachment.client_uuid.as_deref(), Some(&uuid[..]));
        assert_eq!(attachment.fallback_digest.as_deref(), Some(&digest[..]));
        assert_eq!(attachment.fallback_plaintext_hash, None);
        // An imported attachment named only by its plaintext hash.
        let hash = [5u8; 32];
        let imported = vec![crate::receive::media::net_id_plaintext_hash(&hash).unwrap()];
        let imported_delete =
            attachment_message(account, &conv, &from(&user, 78), &imported).expect("a message");
        let by_hash = &imported_delete.attachment_deletes[0];
        assert_eq!(by_hash.fallback_plaintext_hash.as_deref(), Some(&hash[..]));
        assert_eq!(
            (
                by_hash.client_uuid.as_ref(),
                by_hash.fallback_digest.as_ref()
            ),
            (None, None)
        );
        assert_eq!(
            attachment.target_message.as_ref().map(author_and_time),
            Some((ServiceId::from(alice).service_id_binary(), 77))
        );
        // An attachment Kourier itself sent has the name it derived for the pointer.
        let own_names = crate::send::media::attachment_net_ids("1700000000000", 2);
        let own = target(TargetAuthor::Own, "1700000000000");
        let delete = attachment_message(account, &conv, &own, &own_names).expect("a message");
        assert_eq!(
            delete.attachment_deletes[0].client_uuid,
            Some(crate::send::media::client_uuid("1700000000000", 2))
        );
        assert_eq!(delete.attachment_deletes[0].fallback_digest, None);
        // Names of the wrong shape name nothing.
        for bad in [
            vec![],
            vec!["uuid:zz".to_owned()],
            vec!["uuid:0707".to_owned(), "digest:09".to_owned()],
            vec!["hash:00".to_owned()],
            vec![format!("uuid:{}", "é".repeat(16))],
        ] {
            assert_eq!(
                attachment_message(account, &conv, &from(&user, 77), &bad),
                None,
                "{bad:?}"
            );
        }
    }

    /// Both commands are confirmed every time, and the job is stored once per command, also
    /// when the core sends the command again after a restart.
    #[test]
    fn deletes_are_stored_once_and_always_confirmed() {
        let fixture = fixture();
        let account = &fixture.account;
        let alice = Aci::from(Uuid::from_u128(0xa11ce));
        let user = aci_user_id(alice).unwrap();
        let conv = ConvKey::Direct(user.clone());
        let wake = Rc::new(Notify::new());
        let newest = [from(&user, 50)];
        let names = crate::send::media::attachment_net_ids("60", 0);
        fixture
            .db
            .zone(|tx| -> Result<()> {
                let clear = |tx: &mut Tx<'_>, cmd: i64, now: i64| {
                    persist_clear(
                        tx,
                        account,
                        CmdId(cmd),
                        &conv,
                        &newest,
                        &newest,
                        true,
                        now,
                        &wake,
                    )
                };
                assert!(clear(tx, 1, 5)?);
                assert!(!clear(tx, 1, 6)?, "the same command again: one job");
                // Nothing to tell: confirmed without a job.
                assert!(!persist_clear(
                    tx,
                    account,
                    CmdId(2),
                    &conv,
                    &[],
                    &[],
                    false,
                    7,
                    &wake
                )?);
                assert!(persist_attachment(
                    tx,
                    account,
                    CmdId(3),
                    &conv,
                    &target(TargetAuthor::Own, "60"),
                    &names,
                    8,
                    &wake
                )?);
                assert!(!persist_attachment(
                    tx,
                    account,
                    CmdId(4),
                    &conv,
                    &target(TargetAuthor::Own, "60"),
                    &[],
                    9,
                    &wake
                )?);
                assert_eq!(jobs::count(tx, JobKind::DeleteSync)?, 2);
                // The job holds the sync message itself, and runs before catch-up ends.
                let job = jobs::next_due(tx, 10, false, false)?.expect("due while catching up");
                assert_eq!(job.kind, JobKind::DeleteSync);
                assert!(job.key.starts_with("1:"), "{}", job.key);
                let stored = DeleteForMe::decode(job.payload.as_deref().unwrap()).unwrap();
                assert_eq!(stored.conversation_deletes.len(), 1);
                // Once it ran, the command is remembered: a late copy adds nothing.
                jobs::finish(tx, &job, Done::Finished, 11)?;
                assert!(!clear(tx, 1, 12)?);
                assert_eq!(jobs::count(tx, JobKind::DeleteSync)?, 1);
                Ok(())
            })
            .unwrap();
        let confirmed: Vec<i64> = fixture
            .db
            .read(|c| -> Result<Vec<Vec<u8>>> {
                let mut stmt = c.prepare("SELECT envelope FROM core_outbox ORDER BY seq")?;
                let rows = stmt
                    .query_map([], |r| r.get::<_, Vec<u8>>(0))?
                    .collect::<kourier_db::rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .unwrap()
            .iter()
            .flat_map(|row| kourier_backend_api::decode_outbox_row(row).unwrap())
            .filter_map(|event| match event {
                BackendEvent::CommandPersisted { cmd } => Some(cmd.0),
                _ => None,
            })
            .collect();
        assert_eq!(confirmed, [1, 1, 2, 3, 4, 1], "every command, each time");
    }
}
