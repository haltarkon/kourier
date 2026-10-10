//! Outgoing edits, deletes and reactions (§3.5.1-3.5.3, §3.8, §4.8).
//!
//! Each user action is one `core.db` transaction: the target is validated against the
//! conversation's features (edit window and count, delete window, reactions and the per-user
//! limit, admin deletes), the local state changes at once (a revision for an edit, a tombstone
//! for a delete, the own reaction row) and an outbox row of the target message carries the
//! [`OutgoingRequest`] with its own network id, allocated once like a message's (an edit, a
//! delete and a reaction are network messages of their own; Signal gives each a timestamp). The
//! row goes through the conversation's FIFO with the messages, so an edit or a reaction of a
//! message that is still queued is sent after it.
//!
//! When the network refuses a change for good (or its retry budget runs out), the optimistic
//! state is rolled back from the row's `undo` snapshot and the UI gets `ActionFailed`:
//!
//! - an edit loses its revision (the previous text is back);
//! - a reaction gets its previous own state for that emoji back;
//! - a delete for everyone stays a tombstone, but as `ForMe`: its content is gone from this
//!   device already (like Signal Desktop, attachments are deleted at once), and only the claim
//!   that the others lost it too is withdrawn;
//! - a delete for me is local and stays; its sync to our other devices is best effort;
//! - a change that reached some recipients on an earlier, partial attempt stays too (it cannot
//!   be taken back from them), and its failure for the others is not reported.
//!
//! A message that nobody has is edited in place (one that was never attempted, and one the
//! network refused for good, which waits for "Retry"), unless an edit of it waits already:
//! that one was typed when an attempt might have reached the server, so it is a revision, and
//! later edits are revisions behind it. An edit in place is local, so the chat's rules for
//! edits (whether it allows them, its window and count) do not bind it. Deleting a message
//! nobody has cancels it (§4.8 step 5). A message whose send gave up after attempts that got no
//! definite answer (time-outs until the budget ran out) may be with the server all the same:
//! it is edited by a revision, and a delete for everyone of it is sent.
//! Nothing about a message goes to people who never got it: reactions to, and edits queued
//! behind, a message whose own send then failed for everyone stay local and are held in the
//! outbox until the message is retried ([`outbox::take_due`]). A reaction or edit of a message
//! that reached only some members still goes to all of them (as in Signal Desktop).
//!
//! An edit's target is set when it is sent ([`outbox::take_due`]), so an edit typed while an
//! earlier one was on its way, which the network then refused, names the latest revision the
//! recipients have. In Signal's Note to Self an edit has no window and no count
//! ([`actions::edit`]).

use kourier_backend_api::{Network, OutgoingKind, OutgoingRequest, TransmitOutcome};
use kourier_db::rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use kourier_model::{
    AccountId, AccountState, BackendError, Body, ConvKey, ConversationFeatures, ErrorKind,
    MessageId, MsgTarget, NetMsgId, TargetAuthor, Text,
};
use serde::{Deserialize, Serialize};

use crate::actions::{self, Facts, Sending};
use crate::error::{CoreError, Refusal, Result};
use crate::ingest::{Apply, MAX_EMOJI_BYTES, MsgRow};
use crate::outbox::{self, Attempt, Next, Recorded, Written};
use crate::sql;

/// A change of an existing message the user asked for.
#[derive(Clone, Debug)]
pub(crate) enum Change {
    /// Replace the text of an own text message, or the caption of an own message with
    /// attachments.
    Edit(Text),
    /// Delete for everyone (own messages, or others' as a group admin) or for this account.
    Delete {
        /// For everyone.
        for_everyone: bool,
    },
    /// Add or remove an own reaction.
    React {
        /// The emoji.
        emoji: String,
        /// Remove it.
        remove: bool,
    },
}

/// How a permanently failed change is rolled back (`outbox.undo`, JSON).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum Undo {
    /// Drop the revision this edit added.
    Edit {
        /// The edit's network id (`message_revisions.net_local`).
        local: String,
    },
    /// Restore the own reaction row of `emoji` (`None`: there was none).
    React {
        /// The emoji.
        emoji: String,
        /// `at` of the optimistic row (a later change of the row is left alone).
        at: i64,
        /// The previous `(at, removed)`.
        prev: Option<(i64, bool)>,
    },
    /// The tombstone becomes `ForMe`.
    DeleteForEveryone,
    /// Nothing to undo (a delete-for-me sync).
    Nothing,
    /// Some recipients have the change already (an earlier, partial attempt): it cannot be
    /// taken back from them, so a later failure for the others is neither rolled back nor
    /// reported.
    Reached,
}

/// The UI action name of an outbox kind (`ActionFailed.action`).
fn action_of(kind: &str) -> &'static str {
    match kind {
        "Edit" => "EditMessage",
        "Delete" => "DeleteMessage",
        _ => "React",
    }
}

/// The target of one change with everything its rules look at.
struct Target<'a> {
    row: MsgRow,
    key: ConvKey,
    blocked: bool,
    features: Option<ConversationFeatures>,
    network: &'a dyn Network,
    now: i64,
    /// The stored body (latest revision).
    body: Body,
    /// One of our ids is an admin of the conversation.
    admin: bool,
    /// Where the message's own send stands.
    sending: Sending,
    /// The account is linked.
    active: bool,
    /// The chat with ourselves.
    note_to_self: bool,
    /// The message's own send row is still in the outbox.
    queued: bool,
}

impl Target<'_> {
    /// A reference to the original message (deletes and reactions target it).
    fn original(&self) -> Result<MsgTarget> {
        let author = (!self.row.net_author.is_empty())
            .then(|| sql::user(&self.row.net_author))
            .transpose()?;
        let id = NetMsgId {
            author,
            local: self.row.net_local.as_str().into(),
        };
        Ok(MsgTarget::to_id(Some(self.key.clone()), &id))
    }

    /// What [`actions`] decides on: the same facts a timeline row's flags are computed from.
    fn facts(&self) -> Facts<'_> {
        Facts {
            active: self.active,
            blocked: self.blocked,
            features: self.features.as_ref(),
            admin: self.admin,
            own: self.row.is_own(),
            body_kind: &self.row.body_kind,
            deleted: self.row.deleted.as_deref(),
            sent_at: self.row.sent_at,
            revision: self.row.revision,
            media: actions::media_of(&self.body, self.features.as_ref()),
            has_text: actions::has_text(&self.body),
            sending: self.sending,
            note_to_self: self.note_to_self,
            queued: self.queued,
        }
    }
}

/// Applies `change` to `message` and queues it for the network, in one transaction.
pub(crate) fn modify(
    conn: &mut Connection,
    network: &dyn Network,
    message: MessageId,
    change: Change,
    now: i64,
) -> Result<Written> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let found: Option<(i64, String, bool, String, bool)> = tx
        .prepare_cached(
            "SELECT m.account_id, c.conv_key, c.blocked, m.body, c.is_self FROM messages m
             JOIN conversations c ON c.id = m.conversation_id
             WHERE m.id = ?1 AND m.expiry_hidden = 0",
        )?
        .query_row([message.0], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })
        .optional()?;
    // A message hidden because its time has come on an unconfirmed clock is gone for the
    // user ([`crate::expiry`]): a command that still names it is refused like any other
    // about a message that is not there.
    let (account, key, blocked, body, note_to_self) =
        found.ok_or(CoreError::refused(Refusal::Gone, "unknown message"))?;
    let account = AccountId(account);
    let mut apply = Apply::begin(&tx, account, network.descriptor().msg_id_scope, now)?;
    let row = apply
        .msg_row(message)?
        .ok_or(CoreError::refused(Refusal::Gone, "unknown message"))?;
    let features = apply.features(&row)?;
    let admin = apply.own_admin(&row)?;
    let sending = if row.is_own() {
        actions::sending_of(apply.conn, row.id)?
    } else {
        Sending::Settled
    };
    let conv = row.conv;
    let queued = row.is_own() && actions::send_waits(apply.conn, row.id)?;
    let t = Target {
        row,
        key: sql::conv_key(&key)?,
        blocked,
        features,
        network,
        now,
        body: sql::from_json(&body)?,
        admin,
        sending,
        active: apply.acc.state == AccountState::Active,
        note_to_self,
        queued,
    };
    match change {
        Change::Edit(text) => edit(&mut apply, &t, text)?,
        Change::Delete { for_everyone } => delete(&mut apply, &t, for_everyone)?,
        Change::React { emoji, remove } => react(&mut apply, &t, &emoji, remove)?,
    }
    apply.finish()?;
    let changes = std::mem::take(&mut apply.ch);
    drop(apply);
    tx.commit()?;
    Ok(Written {
        account,
        conv,
        changes,
    })
}

/// Inserts the outbox row of a change of `t.row` (state `Queued`, a fresh retry budget).
fn queue(
    apply: &Apply<'_>,
    t: &Target<'_>,
    id: kourier_backend_api::AllocatedId,
    kind: OutgoingKind,
    undo: &Undo,
) -> Result<()> {
    let name = kind.name();
    let req = OutgoingRequest {
        conv: t.key.clone(),
        local: id.local,
        sent_at: id.sent_at,
        kind,
        only: None,
        is_recipient_update: false,
    };
    apply
        .conn
        .prepare_cached(
            "INSERT INTO outbox(message_id, conversation_id, kind, request, state, attempts,
               next_attempt_at, deadline_at, undo)
             VALUES (?1, ?2, ?3, ?4, ?5, 0, NULL, ?6, ?7)",
        )?
        .execute(params![
            t.row.id.0,
            t.row.conv.0,
            name,
            sql::to_json(&req)?,
            outbox::QUEUED,
            t.now.saturating_add(outbox::ms(outbox::BUDGET)),
            sql::to_json(undo)?,
        ])?;
    Ok(())
}

/// The message's own send row: (outbox id, request).
fn message_row(conn: &Connection, id: MessageId) -> Result<Option<(i64, String)>> {
    Ok(conn
        .prepare_cached(
            "SELECT id, request FROM outbox WHERE message_id = ?1 AND kind = 'Message'",
        )?
        .query_row([id.0], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?)
}

// ---------- edit ----------

fn edit(apply: &mut Apply<'_>, t: &Target<'_>, mut text: Text) -> Result<()> {
    let facts = t.facts();
    actions::edit(&facts, t.now)?;
    let row = &t.row;
    text.sanitize();
    let filled = !text.body.trim().is_empty();
    // The stored body of the new revision, and what the network gets: a message with
    // attachments keeps them and only its caption changes (§3.5.3).
    let (body, request) = match &t.body {
        Body::Media { attachments, .. } => {
            if filled && !facts.media.is_some_and(|m| m.captionable) {
                return Err(CoreError::refused(
                    Refusal::NotAllowedHere,
                    "this chat takes no caption with this kind of file",
                ));
            }
            let caption = filled.then(|| text.clone());
            let files = apply.request_attachments(row.id, attachments)?;
            let request = if files.is_empty() {
                // Only a long text beside it: for the network this is a text message.
                if !filled {
                    return Err(CoreError::refused(Refusal::Empty, "the message is empty"));
                }
                Body::Text(text.clone())
            } else {
                Body::Media {
                    attachments: files,
                    caption: caption.clone(),
                }
            };
            let stored = Body::Media {
                attachments: attachments.clone(),
                caption,
            };
            (stored, request)
        }
        _ if !filled => return Err(CoreError::refused(Refusal::Empty, "the message is empty")),
        _ => (Body::Text(text.clone()), Body::Text(text.clone())),
    };
    if t.features.as_ref().is_some_and(|f| {
        f.max_text_bytes
            .is_some_and(|max| text.body.len() > usize::try_from(max).unwrap_or(usize::MAX))
    }) {
        return Err(CoreError::refused(
            Refusal::TooLong,
            "the text is too long for this chat",
        ));
    }
    if t.body == body {
        return Err(CoreError::refused(
            Refusal::Unchanged,
            "the text did not change",
        ));
    }
    let json = sql::to_json(&body)?;

    // §4.8 step 5: a message nobody has is edited in place, so nobody ever sees the earlier
    // text: one that was never attempted, and one the network refused for good (its row
    // waits for "Retry", which then sends the new text). One that an attempt may have reached
    // the server with (it waits for a retry, or its budget ran out on time-outs) gets a
    // revision instead, sent behind it.
    if facts.edits_in_place()
        && let Some((outbox_id, queued)) = message_row(apply.conn, row.id)?
    {
        let mut req: OutgoingRequest = sql::from_json(&queued)?;
        if let OutgoingKind::Message { body: b, .. } = &mut req.kind {
            match (b, &body) {
                // The queued request keeps its files (with their blob references).
                (Body::Media { caption: c, .. }, Body::Media { caption: new, .. }) => {
                    c.clone_from(new);
                }
                (b, _) => *b = request,
            }
        }
        apply
            .conn
            .prepare_cached(
                "UPDATE messages SET body = ?2, body_kind = ?3, preview = ?4 WHERE id = ?1",
            )?
            .execute(params![
                row.id.0,
                json,
                body.kind_name(),
                crate::ingest::preview(&body)
            ])?;
        apply
            .conn
            .prepare_cached("UPDATE outbox SET request = ?2 WHERE id = ?1")?
            .execute((outbox_id, sql::to_json(&req)?))?;
        apply.ch.message(row.conv, row.id);
        return Ok(());
    }

    let id = outbox::allocate(apply, t.network, row.conv, &t.key, t.now)?;
    // Signal: the edit targets the previous revision's timestamp
    // (`SD/ts/util/editHelpers.std.ts`), which is the latest one we have.
    let previous: Option<String> = apply
        .conn
        .prepare_cached(
            "SELECT net_local FROM message_revisions WHERE message_id = ?1
             ORDER BY revision DESC LIMIT 1",
        )?
        .query_row([row.id.0], |r| r.get(0))
        .optional()?;
    let target = MsgTarget {
        conv: Some(t.key.clone()),
        author: TargetAuthor::Own,
        local: previous.unwrap_or_else(|| row.net_local.clone()).into(),
        alternates: Vec::new(),
    };
    // The original becomes revision 0 (once), the edit the latest revision (§3.5.3).
    apply
        .conn
        .prepare_cached(
            "INSERT OR IGNORE INTO message_revisions(message_id, account_id, net_author,
               net_local, revision, body, edited_at)
             SELECT id, account_id, net_author, net_local, 0, body, NULL
             FROM messages WHERE id = ?1",
        )?
        .execute([row.id.0])?;
    apply
        .conn
        .prepare_cached(
            "INSERT INTO message_revisions(message_id, account_id, net_author, net_local,
               revision, body, edited_at) VALUES (?1, ?2, '', ?3, 0, ?4, ?5)",
        )?
        .execute(params![
            row.id.0,
            apply.acc.id.0,
            id.local.as_ref(),
            json,
            id.sent_at
        ])?;
    apply.reindex_revisions(row.id)?;
    apply.ch.message(row.conv, row.id);
    let undo = Undo::Edit {
        local: id.local.to_string(),
    };
    let kind = OutgoingKind::Edit {
        target,
        body: request,
    };
    queue(apply, t, id, kind, &undo)
}

// ---------- delete ----------

fn delete(apply: &mut Apply<'_>, t: &Target<'_>, for_everyone: bool) -> Result<()> {
    let (how, _) = actions::delete(&t.facts(), for_everyone, t.now)?;
    let row = &t.row;
    if how == actions::Delete::TakeBack {
        // §4.8 step 5: nobody has it, so it is simply taken back.
        apply.delete_attachments(row.id)?;
        apply
            .conn
            .prepare_cached("DELETE FROM messages WHERE id = ?1")?
            .execute([row.id.0])?;
        apply.recount(row.conv)?;
        apply.ch.conversations.insert(row.conv);
        apply.ch.timelines.insert(row.conv);
        return Ok(());
    }
    // Some (may) have it: the row kept for retrying the others goes with the message, and so
    // do queued edits and reactions, which are moot now.
    apply
        .conn
        .prepare_cached(
            "DELETE FROM outbox WHERE message_id = ?1 AND kind IN ('Message', 'Edit', 'React')
               AND state <> 'Transmitting'",
        )?
        .execute([row.id.0])?;
    let target = t.original()?;
    if for_everyone {
        let id = outbox::allocate(apply, t.network, row.conv, &t.key, t.now)?;
        apply.tombstone(row, "ForEveryone", "")?;
        let kind = OutgoingKind::Delete {
            target,
            for_everyone: true,
        };
        return queue(apply, t, id, kind, &Undo::DeleteForEveryone);
    }
    // Nothing of a call line is ever sent: it is removed here, and our other devices keep
    // their call entry (a delete of a call is its own sync on Signal, not `deleteForMe`).
    let call_line = crate::ingest::is_call_line(apply.conn, row.id)?;
    apply.tombstone(row, "ForMe", "")?;
    // Our other devices learn it when the network syncs deletes for me (Signal `deleteForMe`).
    let syncs = t.features.as_ref().is_none_or(|f| f.delete_for_me);
    if t.active && syncs && !call_line {
        let id = outbox::allocate(apply, t.network, row.conv, &t.key, t.now)?;
        let kind = OutgoingKind::Delete {
            target,
            for_everyone: false,
        };
        queue(apply, t, id, kind, &Undo::Nothing)?;
    }
    Ok(())
}

// ---------- react ----------

fn react(apply: &mut Apply<'_>, t: &Target<'_>, emoji: &str, remove: bool) -> Result<()> {
    let per_user = actions::react(&t.facts())?;
    let row = &t.row;
    if emoji.trim().is_empty() || emoji.len() > MAX_EMOJI_BYTES {
        return Err(CoreError::InvalidInput("not a reaction"));
    }
    // Our reactions as the timeline shows them (the query's rule): newest first; with one per
    // user the newest entry is the state, a removal included; in a set the newest active ones.
    let own: Vec<(String, bool)> = apply
        .conn
        .prepare_cached(
            "SELECT emoji, removed FROM reactions WHERE message_id = ?1 AND user_id = ''
             ORDER BY at DESC, removed DESC, emoji",
        )?
        .query_map([row.id.0], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<kourier_db::rusqlite::Result<_>>()?;
    let shown: Vec<&str> = if per_user == Some(1) {
        own.first()
            .filter(|(_, removed)| !removed)
            .map(|(e, _)| e.as_str())
            .into_iter()
            .collect()
    } else {
        own.iter()
            .filter(|(_, removed)| !removed)
            .take(per_user.map_or(usize::MAX, usize::from))
            .map(|(e, _)| e.as_str())
            .collect()
    };
    let has = shown.contains(&emoji);
    if has != remove {
        // Already there, or nothing to remove.
        return Ok(());
    }
    if !remove && per_user.is_some_and(|n| n > 1 && shown.len() >= usize::from(n)) {
        return Err(CoreError::refused(
            Refusal::TooMany,
            "you have as many reactions on this message as the chat allows",
        ));
    }
    let prev: Option<(i64, bool)> = apply
        .conn
        .prepare_cached(
            "SELECT at, removed FROM reactions WHERE message_id = ?1 AND user_id = ''
               AND emoji = ?2",
        )?
        .query_row((row.id.0, emoji), |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?;
    let id = outbox::allocate(apply, t.network, row.conv, &t.key, t.now)?;
    // The id's timestamp is strictly increasing per account, so this write wins (LWW by `at`).
    let at = id
        .sent_at
        .max(prev.map_or(i64::MIN, |p| p.0.saturating_add(1)));
    apply.merge_reaction(row.id, "", emoji, at, remove)?;
    apply.ch.message(row.conv, row.id);
    let undo = Undo::React {
        emoji: emoji.to_owned(),
        at,
        prev,
    };
    let kind = OutgoingKind::React {
        target: t.original()?,
        emoji: emoji.to_owned(),
        remove,
    };
    queue(apply, t, id, kind, &undo)
}

// ---------- outcomes ----------

/// §4.8 step 3 for an edit, delete or reaction row (inside [`outbox::record_outcome`]'s
/// transaction): done when the network has it (for some recipients at least, if the others
/// failed for good), retried with Signal's backoff inside the budget (only the members a partial
/// attempt missed), paused like a message, or rolled back when refused for good.
pub(crate) fn record_outcome(
    apply: &mut Apply<'_>,
    attempt: &Attempt,
    outcome: Option<TransmitOutcome>,
) -> Result<Recorded> {
    let outbox_id = attempt.outbox;
    // Outbox ids are never reused, so a row with this id is the row of this attempt; a merge
    // of two conversations may have moved it and its target meanwhile (§3.3).
    type Row = (i64, i64, String, Option<String>, i64, i64);
    let row: Option<Row> = apply
        .conn
        .prepare_cached(
            "SELECT attempts, deadline_at, kind, undo, message_id, conversation_id FROM outbox
             WHERE id = ?1 AND state = ?2",
        )?
        .query_row((outbox_id, outbox::TRANSMITTING), |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
            ))
        })
        .optional()?;
    let mut recorded = Recorded::default();
    // The row went with its target while the attempt was in flight (the message expired, its
    // conversation was removed): nothing is recorded, nothing rolled back.
    let Some((attempts, deadline, kind, undo, message, conv)) = row else {
        return Ok(recorded);
    };
    let (message, conv) = (MessageId(message), kourier_model::ConversationId(conv));
    let attempts = sql::u32_of(attempts);
    let now = apply.now;
    let outcome = outcome.unwrap_or_else(|| {
        TransmitOutcome::Failed(BackendError::new(
            ErrorKind::Transient { retry_after: None },
            "the backend dropped the transmit reply",
        ))
    });
    let requeue = |apply: &Apply<'_>, wait: std::time::Duration, req: &OutgoingRequest, error| {
        apply
            .conn
            .prepare_cached(
                "UPDATE outbox SET state = ?2, next_attempt_at = ?3, request = ?4,
                   last_error = ?5 WHERE id = ?1",
            )?
            .execute(params![
                outbox_id,
                outbox::QUEUED,
                now.saturating_add(outbox::ms(wait)),
                sql::to_json(req)?,
                error
            ])?;
        Ok::<_, CoreError>(())
    };
    let failure = match outcome {
        TransmitOutcome::Sent { .. } => None,
        TransmitOutcome::Partial { delivered, failed } => {
            // A member the network wants a challenge solved for, or whose safety number
            // changed, is not lost: the row waits, narrowed to the members that do not have
            // the change, until the user answered (the account's pause) or confirmed the
            // number (the conversation's), like a whole send that ended that way.
            let pause = match (outbox::challenge_of(&failed), outbox::untrusted_of(&failed)) {
                (Some(challenge), _) => Some((
                    outbox::Pause::Account {
                        challenge: Some(challenge),
                    },
                    "Challenge",
                )),
                (None, Some(user)) => Some((
                    outbox::Pause::Conversation {
                        conv,
                        outbox: outbox_id,
                        user,
                    },
                    "UntrustedIdentity",
                )),
                (None, None) => None,
            };
            // A change past its budget had its one late attempt (`outbox::record_outcome`):
            // it does not wait for the user on top. What reached some members stands; else
            // it is rolled back and reported. The account still waits on a challenge.
            let late = now > deadline;
            if late && let Some((pause, _)) = &pause {
                if matches!(pause, outbox::Pause::Account { .. }) {
                    recorded.pause = Some(pause.clone());
                }
                let failure = match delivered.is_empty() {
                    true => failed
                        .into_iter()
                        .find(|(_, e)| outbox::waits_for_user(e))
                        .map(|(_, e)| e),
                    false => None,
                };
                apply
                    .conn
                    .prepare_cached("DELETE FROM outbox WHERE id = ?1")?
                    .execute([outbox_id])?;
                if let Some(e) = failure {
                    recorded.failed = refused(apply, message, &kind, undo, e)?;
                }
                return Ok(recorded);
            }
            if let Some((pause, name)) = pause {
                let members = failed
                    .iter()
                    .filter(|(_, e)| outbox::waits_for_user(e))
                    .map(|(u, _)| u.clone())
                    .collect();
                apply
                    .conn
                    .prepare_cached(
                        "UPDATE outbox SET state = ?2, next_attempt_at = NULL, request = ?3,
                           last_error = ?4 WHERE id = ?1",
                    )?
                    .execute(params![
                        outbox_id,
                        outbox::QUEUED,
                        sql::to_json(&attempt.req.resend_to(members))?,
                        name
                    ])?;
                if !delivered.is_empty() {
                    apply
                        .conn
                        .prepare_cached("UPDATE outbox SET undo = ?2 WHERE id = ?1")?
                        .execute((outbox_id, sql::to_json(&Undo::Reached)?))?;
                }
                recorded.pause = Some(pause);
                return Ok(recorded);
            }
            let retry: Vec<&(kourier_model::UserId, BackendError)> =
                failed.iter().filter(|(_, e)| e.is_retryable()).collect();
            let wait = retry
                .iter()
                .map(|(_, e)| e.kind.retry_after().unwrap_or_default())
                .max()
                .unwrap_or_default()
                .max(outbox::backoff(attempts));
            match retry.first() {
                Some((_, first)) if now.saturating_add(outbox::ms(wait)) <= deadline => {
                    let members = retry.iter().map(|(u, _)| u.clone()).collect();
                    requeue(
                        apply,
                        wait,
                        &attempt.req.resend_to(members),
                        first.kind.name(),
                    )?;
                    if !delivered.is_empty() {
                        apply
                            .conn
                            .prepare_cached("UPDATE outbox SET undo = ?2 WHERE id = ?1")?
                            .execute((outbox_id, sql::to_json(&Undo::Reached)?))?;
                    }
                    return Ok(recorded);
                }
                // Some have it: a change cannot be taken back from them.
                _ if !delivered.is_empty() => None,
                _ => failed.into_iter().next().map(|(_, e)| e),
            }
        }
        TransmitOutcome::Failed(e) => match outbox::classify(&e, conv, outbox_id, attempts) {
            Next::Retry(wait) if now.saturating_add(outbox::ms(wait)) <= deadline => {
                requeue(apply, wait, &attempt.req, e.kind.name())?;
                return Ok(recorded);
            }
            // Past its budget, this was its one late attempt: it is rolled back and
            // reported instead of waiting for the user. The account still waits on a
            // challenge.
            n @ Next::Pause(_) if now > deadline && outbox::late_pause(&n) => {
                if let Next::Pause(p @ outbox::Pause::Account { .. }) = n {
                    recorded.pause = Some(p);
                }
                Some(e)
            }
            Next::Pause(p) => {
                apply
                    .conn
                    .prepare_cached(
                        "UPDATE outbox SET state = ?2, next_attempt_at = NULL, last_error = ?3
                             WHERE id = ?1",
                    )?
                    .execute(params![outbox_id, outbox::QUEUED, e.kind.name()])?;
                recorded.pause = Some(p);
                return Ok(recorded);
            }
            Next::Retry(_) | Next::Fail { .. } => Some(e),
        },
    };
    apply
        .conn
        .prepare_cached("DELETE FROM outbox WHERE id = ?1")?
        .execute([outbox_id])?;
    if let Some(e) = failure {
        recorded.failed = refused(apply, message, &kind, undo, e)?;
    }
    Ok(recorded)
}

/// A change the network will not get (refused for good, or out of budget): its optimistic
/// local state is rolled back from the row's `undo` snapshot. Returns what to report.
fn refused(
    apply: &mut Apply<'_>,
    message: MessageId,
    kind: &str,
    undo: Option<String>,
    e: BackendError,
) -> Result<Option<outbox::FailedChange>> {
    let undo: Undo = sql::from_json_opt(undo)?.unwrap_or(Undo::Nothing);
    tracing::info!(
        message = message.0,
        kind,
        error = e.kind.name(),
        kept = matches!(undo, Undo::Nothing | Undo::Reached),
        "the network refused a change"
    );
    // The user deleted the message while its edit or reaction was on the way (or hid the
    // tombstone of a delete for everyone that then failed): the refusal is about something
    // that is gone for them, so it is not reported. A refused delete for everyone whose
    // tombstone still shows is.
    let gone = apply.msg_row(message)?.is_none_or(|row| {
        let own_tombstone = kind == "Delete" && row.deleted.as_deref() == Some("ForEveryone");
        row.deleted.is_some() && !own_tombstone
    });
    roll_back(apply, message, &undo)?;
    // A delete-for-me sync is best effort: the local delete stands, nothing to report; a
    // change some recipients have stands as well.
    if gone || matches!(undo, Undo::Nothing | Undo::Reached) {
        return Ok(None);
    }
    Ok(Some(outbox::FailedChange {
        action: action_of(kind),
        message,
        error: e,
    }))
}

/// A queued edit, delete or reaction gives up without another attempt (its budget ended
/// while the account waited for a challenge, [`outbox::give_up_overdue`]): the row goes and
/// the change is rolled back like one the network refused. Returns what to report.
pub(crate) fn give_up(
    apply: &mut Apply<'_>,
    outbox_id: i64,
    e: BackendError,
) -> Result<Option<outbox::FailedChange>> {
    let row: Option<(String, Option<String>, i64)> = apply
        .conn
        .prepare_cached("SELECT kind, undo, message_id FROM outbox WHERE id = ?1 AND state = ?2")?
        .query_row((outbox_id, outbox::QUEUED), |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .optional()?;
    let Some((kind, undo, message)) = row else {
        return Ok(None);
    };
    apply
        .conn
        .prepare_cached("DELETE FROM outbox WHERE id = ?1")?
        .execute([outbox_id])?;
    refused(apply, MessageId(message), &kind, undo, e)
}

/// Undoes the optimistic local state of a refused change.
fn roll_back(apply: &mut Apply<'_>, message: MessageId, undo: &Undo) -> Result<()> {
    let Some(row) = apply.msg_row(message)? else {
        return Ok(());
    };
    match undo {
        Undo::Nothing | Undo::Reached => return Ok(()),
        Undo::Edit { local } => {
            apply
                .conn
                .prepare_cached(
                    "DELETE FROM message_revisions WHERE message_id = ?1 AND net_author = ''
                       AND net_local = ?2",
                )?
                .execute((message.0, local))?;
            apply.reindex_revisions(message)?;
            // Only the original is left: the message is unedited again.
            apply
                .conn
                .prepare_cached(
                    "DELETE FROM message_revisions WHERE message_id = ?1
                       AND (SELECT count(*) FROM message_revisions WHERE message_id = ?1) = 1",
                )?
                .execute([message.0])?;
        }
        Undo::React { emoji, at, prev } => {
            if row.deleted.is_some() {
                return Ok(());
            }
            match prev {
                None => apply
                    .conn
                    .prepare_cached(
                        "DELETE FROM reactions WHERE message_id = ?1 AND user_id = ''
                           AND emoji = ?2 AND at = ?3",
                    )?
                    .execute(params![message.0, emoji, at])?,
                Some((prev_at, removed)) => apply
                    .conn
                    .prepare_cached(
                        "UPDATE reactions SET at = ?4, removed = ?5 WHERE message_id = ?1
                           AND user_id = '' AND emoji = ?2 AND at = ?3",
                    )?
                    .execute(params![message.0, emoji, at, prev_at, removed])?,
            };
        }
        Undo::DeleteForEveryone => {
            apply
                .conn
                .prepare_cached(
                    "UPDATE messages SET deleted = 'ForMe' WHERE id = ?1
                       AND deleted = 'ForEveryone' AND deleted_by = ''",
                )?
                .execute([message.0])?;
        }
    }
    apply.ch.message(row.conv, message);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn undo_snapshots_round_trip() {
        for u in [
            Undo::Edit { local: "17".into() },
            Undo::React {
                emoji: "👍".into(),
                at: 5,
                prev: Some((3, true)),
            },
            Undo::DeleteForEveryone,
            Undo::Nothing,
            Undo::Reached,
        ] {
            let json = sql::to_json(&u).expect("json");
            assert_eq!(sql::from_json::<Undo>(&json).expect("parse"), u);
        }
        assert_eq!(action_of("Edit"), "EditMessage");
        assert_eq!(action_of("Delete"), "DeleteMessage");
        assert_eq!(action_of("React"), "React");
        assert_eq!(actions::age_secs(10_000, 4_000), 0);
        assert_eq!(actions::age_secs(0, 61_999), 61);
    }
}
