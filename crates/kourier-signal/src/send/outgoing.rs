//! What one `Command::Transmit` puts on the wire (§6.7 step 1, S13): a message with its quote,
//! an edit, a delete for everyone, a reaction, or the delete-for-me sync.
//!
//! Ported from Desktop's send jobs: `SD/ts/jobs/helpers/sendNormalMessage.preload.ts` (messages
//! and edits, `getMessageSendData`, `uploadMessageQuote`), `sendReaction.preload.ts`,
//! `sendDeleteForEveryone.preload.ts`, and `SD/ts/textsecure/SendMessage.preload.ts`
//! `getDeleteForMeSyncMessage` with `SD/ts/util/syncIdentifiers.preload.ts`.
//!
//! - **Edit**: a new `DataMessage` with the request's own timestamp inside an `EditMessage`
//!   whose `targetSentTimestamp` is the request's target (the previous revision). It carries
//!   the chat's timer, the attachments of the message (never changed by an edit) and the quote
//!   the message was sent with: receivers take the quote of an edit as the message's quote
//!   (`SD/ts/util/handleEditMessage.preload.ts`), and the core's edit request does not name
//!   it, so the backend keeps the quote of each reply our account sent (`sent_quotes`: from
//!   here, or from another device of ours by its sync transcript) and hands it on from
//!   revision to revision.
//! - **Delete for everyone**: `DataMessage.delete` for an own message; in a group, an admin's
//!   delete of someone else's message is `DataMessage.adminDelete` with the author.
//! - **Reaction**: `DataMessage.reaction` with the target's author and timestamp, the chat's
//!   timer and (1:1) its version. Only one emoji is sent ([`crate::emoji`]): the receivers
//!   drop anything else.
//! - **Delete for me**: nothing goes to the chat; our other devices get
//!   `SyncMessage.deleteForMe` naming the conversation and the message.
//!
//! The caller sends the result like a message: sealed 1:1 or over sender keys, logged in the
//! Message Send Log, with a sync transcript (`SyncMessage.Sent.message` or `.editMessage`) and
//! the per-recipient outcome of §4.8.

use std::time::Duration;

use kourier_backend_api::{OutgoingKind, OutgoingRequest, TransmitOutcome};
use kourier_db::ZoneDb;
use kourier_db::rusqlite::{Connection, OptionalExtension};
use kourier_model::{BackendError, ErrorKind, MsgTarget, TargetAuthor};
use libsignal_core::{Aci, ServiceId};
use prost::Message as _;

use super::content::{self, GroupContext, Reaction, TextMessage, Thread, Wire};
use super::media::Uploader;
use super::{Delivery, Outgoing, SendContext, send_to};
use crate::db::Account;
use crate::error::Result;
use crate::net::SignalTransport;
use crate::proto;
use crate::service_id::{SignalUser, parse_user_id};

/// How long the quote of a sent reply is kept for its edits: longer than the 24 h edit window.
/// (Note to Self has no window; an older reply edited there goes out without its quote.)
const QUOTE_RETENTION: Duration = Duration::from_secs(14 * 24 * 3600);

/// The chat a request is sent in.
#[derive(Clone, Copy)]
pub(crate) struct Chat<'a> {
    /// Our account.
    pub(crate) account: &'a Account,
    /// The chat's disappearing-message timer in seconds, 0 = off.
    pub(crate) timer: u32,
    /// The timer version of a 1:1 chat; `None` in a group.
    pub(crate) timer_version: Option<u32>,
    /// The group, for a group chat.
    pub(crate) group: Option<GroupContext<'a>>,
}

/// What to send for a request.
#[derive(Debug)]
pub(crate) struct Built {
    /// The `DataMessage` or `EditMessage`.
    pub(crate) wire: Wire,
    /// It is a message with a timer: its transcript says when the timer started.
    pub(crate) expires: bool,
    /// How it goes out.
    pub(crate) delivery: Delivery,
}

fn invalid(what: &'static str) -> BackendError {
    BackendError::new(ErrorKind::InvalidInput, what)
}

/// The sent timestamp a target names (Signal ids are timestamps).
pub(crate) fn target_timestamp(target: &MsgTarget) -> std::result::Result<u64, BackendError> {
    target
        .local
        .parse()
        .map_err(|_| invalid("the target message has no Signal id"))
}

/// The ACI of a target's author (`own` for our messages).
pub(crate) fn target_author(
    target: &MsgTarget,
    own: Aci,
) -> std::result::Result<Aci, BackendError> {
    match &target.author {
        TargetAuthor::Own => Ok(own),
        TargetAuthor::User(user) => match parse_user_id(user) {
            Ok(SignalUser::Aci(aci)) => Ok(aci),
            _ => Err(invalid("the target's author is not a Signal account")),
        },
        TargetAuthor::Unknown => Err(invalid("the target message has no author")),
    }
}

/// The quote our message `local` went out with, if it was a reply.
pub(crate) fn stored_quote(
    conn: &Connection,
    local: &str,
) -> Result<Option<proto::data_message::Quote>> {
    let bytes: Option<Vec<u8>> = conn
        .prepare_cached("SELECT quote FROM sent_quotes WHERE local = ?1")?
        .query_row([local], |r| r.get(0))
        .optional()?;
    Ok(bytes.and_then(|bytes| proto::data_message::Quote::decode(&bytes[..]).ok()))
}

/// Remembers the quote of our message (or edit) `local`: one Kourier sent, or one another
/// device of ours sent and its sync transcript brought ([`keep_own_quote`]).
pub(crate) fn store_quote(
    conn: &Connection,
    local: &str,
    quote: &proto::data_message::Quote,
    now: i64,
) -> Result<()> {
    conn.prepare_cached(
        "INSERT INTO sent_quotes(local, quote, created_at) VALUES (?1, ?2, ?3)
         ON CONFLICT(local) DO UPDATE SET quote = excluded.quote",
    )?
    .execute((local, quote.encode_to_vec(), now))?;
    Ok(())
}

/// Keeps the quote of a reply (or of an edit of one) that our account sent from another
/// device at `timestamp`, as its sync transcript states it: Kourier may edit that message
/// later, and its edit must carry the quote like the edit of a reply sent from here (module
/// docs), or every receiver would drop the quote, the phone included.
pub(crate) fn keep_own_quote(
    conn: &Connection,
    message: &proto::DataMessage,
    timestamp: u64,
    now: i64,
) -> Result<()> {
    match &message.quote {
        Some(quote) => store_quote(conn, &timestamp.to_string(), quote, now),
        None => Ok(()),
    }
}

/// Forgets quotes older than [`QUOTE_RETENTION`]; returns how many.
pub(crate) fn purge_quotes(conn: &Connection, now: i64) -> Result<usize> {
    let age = i64::try_from(QUOTE_RETENTION.as_millis()).unwrap_or(i64::MAX);
    Ok(conn
        .prepare_cached("DELETE FROM sent_quotes WHERE created_at < ?1")?
        .execute([now.saturating_sub(age)])?)
}

/// Builds what `req` sends in `chat` (module docs), uploading what its body needs. Not for a
/// delete for me ([`sync_delete_for_me`]).
pub(crate) async fn build<T: SignalTransport>(
    db: &ZoneDb,
    uploader: &Uploader<'_, T>,
    chat: &Chat<'_>,
    req: &OutgoingRequest,
    now: i64,
) -> std::result::Result<Built, BackendError> {
    let timestamp = u64::try_from(req.sent_at).map_err(|_| invalid("negative sent_at"))?;
    let account = chat.account;
    let profile_key = &account.profile_key;
    let db_error = |e: crate::error::Error| e.to_backend_error();
    let in_chat = |message: proto::DataMessage| match chat.group {
        Some(group) => content::in_group(message, group),
        None => message,
    };
    let version = chat.timer_version.unwrap_or(0);
    Ok(match &req.kind {
        OutgoingKind::Message {
            body,
            reply_to,
            expiry,
        } => {
            let prepared = uploader.prepare(body).await?;
            let quote = match reply_to {
                Some(reply) => Some(uploader.quote(reply, account.aci).await?),
                None => None,
            };
            if let Some(quote) = &quote {
                db.zone(|tx| store_quote(tx, &req.local, quote, now))
                    .map_err(db_error)?;
            }
            // A 1:1 message carries the timer it was queued with (the core's `expiry`, set from
            // the chat's timer at enqueue, as Desktop's `message.expireTimer`); a group message
            // the group's.
            let expire_timer = match chat.group {
                Some(_) => chat.timer,
                None => expiry
                    .map(|d| u32::try_from(d.as_secs()).unwrap_or(u32::MAX))
                    .unwrap_or(0),
            };
            let message = content::text_message(&TextMessage {
                body: &prepared,
                timestamp,
                profile_key,
                expire_timer,
                expire_timer_version: version,
                quote: quote.as_ref(),
            });
            Built {
                wire: Wire::Data(in_chat(message)),
                expires: expire_timer > 0,
                delivery: Delivery::MESSAGE,
            }
        }
        OutgoingKind::Edit { target, body } => {
            if !matches!(target.author, TargetAuthor::Own) {
                return Err(invalid("only our own messages can be edited"));
            }
            let target_timestamp = target_timestamp(target)?;
            let prepared = uploader.prepare_edit(body).await?;
            let quote = db
                .read(|c| stored_quote(c, &target.local))
                .map_err(db_error)?;
            if let Some(quote) = &quote {
                // The next edit targets this revision.
                db.zone(|tx| store_quote(tx, &req.local, quote, now))
                    .map_err(db_error)?;
            }
            let message = content::text_message(&TextMessage {
                body: &prepared,
                timestamp,
                profile_key,
                expire_timer: chat.timer,
                expire_timer_version: version,
                quote: quote.as_ref(),
            });
            Built {
                wire: Wire::Edit(content::edit_message(target_timestamp, in_chat(message))),
                expires: false,
                delivery: Delivery::MODIFIER,
            }
        }
        OutgoingKind::Delete {
            target,
            for_everyone: true,
        } => {
            let target_timestamp = target_timestamp(target)?;
            let author = target_author(target, account.aci)?;
            let message = if author == account.aci {
                content::delete_message(target_timestamp, timestamp, profile_key)
            } else if chat.group.is_some() {
                content::admin_delete_message(author, target_timestamp, timestamp, profile_key)
            } else {
                return Err(invalid("only our own messages can be deleted for everyone"));
            };
            Built {
                wire: Wire::Data(in_chat(message)),
                expires: false,
                delivery: Delivery::MODIFIER,
            }
        }
        OutgoingKind::Delete {
            for_everyone: false,
            ..
        } => {
            return Err(BackendError::internal(
                "a delete for me is a sync message, not a send",
            ));
        }
        OutgoingKind::React {
            target,
            emoji,
            remove,
        } => {
            // The receivers drop a reaction that is not one emoji ([`crate::emoji`]).
            if !crate::emoji::is_one(emoji) {
                return Err(invalid("a reaction is one emoji"));
            }
            let reaction = Reaction {
                emoji,
                remove: *remove,
                target_author: target_author(target, account.aci)?,
                target_timestamp: target_timestamp(target)?,
            };
            let message =
                content::reaction_message(&reaction, timestamp, profile_key, chat.timer, version);
            Built {
                wire: Wire::Data(in_chat(message)),
                expires: false,
                delivery: Delivery::MODIFIER,
            }
        }
    })
}

/// A delete for me (`OutgoingKind::Delete { for_everyone: false }`): `SyncMessage.deleteForMe`
/// for `target` in `thread` to our other devices, unsealed and not urgent (Desktop's
/// `deleteForMeSync` job). Nobody else hears of it, so the outcome names no recipient.
pub(crate) async fn sync_delete_for_me<T: SignalTransport>(
    ctx: &SendContext<'_, T>,
    thread: &Thread,
    target: &MsgTarget,
    req: &OutgoingRequest,
) -> std::result::Result<TransmitOutcome, BackendError> {
    let timestamp = u64::try_from(req.sent_at).map_err(|_| invalid("negative sent_at"))?;
    let author = ServiceId::from(target_author(target, ctx.account.aci)?);
    let sync = content::delete_for_me(thread, &[(author, target_timestamp(target)?)]);
    send_to(
        ctx,
        ServiceId::from(ctx.account.aci),
        &Outgoing {
            content: &sync,
            timestamp,
            delivery: Delivery::TRANSCRIPT,
            local: Some(&req.local),
            group: None,
        },
    )
    .await
    .map_err(|e| e.to_backend_error())?;
    Ok(TransmitOutcome::Sent {
        server_at: None,
        per_recipient: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use kourier_model::ConvKey;

    use super::*;

    #[test]
    fn targets_are_timestamps_with_an_aci_author() {
        let own = Aci::from(uuid::Uuid::from_u128(1));
        let peer = Aci::from(uuid::Uuid::from_u128(2));
        let user = crate::service_id::aci_user_id(peer).unwrap();
        let target = |author: TargetAuthor, local: &str| MsgTarget {
            conv: Some(ConvKey::Direct(user.clone())),
            author,
            local: local.into(),
            alternates: Vec::new(),
        };
        let ours = target(TargetAuthor::Own, "1700000000000");
        assert_eq!(target_timestamp(&ours).unwrap(), 1_700_000_000_000);
        assert_eq!(target_author(&ours, own).unwrap(), own);
        let theirs = target(TargetAuthor::User(user.clone()), "5");
        assert_eq!(target_author(&theirs, own).unwrap(), peer);
        // A backend-made id (a group change, a timer notice) is no message to modify.
        let notice = target(TargetAuthor::User(user.clone()), "timer:5");
        assert_eq!(
            target_timestamp(&notice).unwrap_err().kind,
            ErrorKind::InvalidInput
        );
        assert!(target_author(&target(TargetAuthor::Unknown, "5"), own).is_err());
    }

    #[test]
    fn quotes_of_sent_replies_are_kept_for_their_edits_and_purged() {
        let fixture = crate::receive::tests::fixture();
        let quote = content::quote(7, Aci::from(uuid::Uuid::from_u128(2)), None, None);
        fixture
            .db
            .zone(|tx| -> Result<()> {
                assert_eq!(stored_quote(tx, "100")?, None);
                store_quote(tx, "100", &quote, 1_000)?;
                store_quote(tx, "100", &quote, 2_000)?;
                assert_eq!(stored_quote(tx, "100")?, Some(quote.clone()));
                let age = i64::try_from(QUOTE_RETENTION.as_millis()).unwrap();
                assert_eq!(
                    purge_quotes(tx, 1_000 + age)?,
                    0,
                    "kept through its retention"
                );
                assert_eq!(purge_quotes(tx, 1_001 + age)?, 1);
                assert_eq!(stored_quote(tx, "100")?, None);
                Ok(())
            })
            .unwrap();
    }
}
