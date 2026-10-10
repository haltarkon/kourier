//! Idempotent ingest (§4.9): applies the durable events of one `Envelope` (or one history chunk)
//! to `core.db` in one transaction and reports what changed, for the `UiEvent` invalidations sent
//! after the commit.
//!
//! Every rule is written so that applying an event twice changes nothing and, for the events
//! scripts may reorder (messages, modifiers, merges), any arrival order converges to the same
//! state: modifiers that arrive before their target wait in `pending_modifiers`; reactions are
//! last-writer-wins by `at` with tombstones; receipts keep the earliest `at`; per-recipient send
//! states and read states only move forward; unread counters and the aggregated outgoing state
//! are recomputed from rows; read markers from other devices are a `sent_at` watermark; the
//! signals of a call join into one line ([`call`]). Only the `sort_key` of live messages depends
//! on arrival order, by design (§3.9).

mod attachment;
mod call;
mod clear;
mod merge;
mod message;
mod modifier;
mod roaming;

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use kourier_backend_api::{BackendEvent, BackendNotice};
use kourier_db::failpoint;
use kourier_db::rusqlite::{Connection, OptionalExtension, TransactionBehavior};
use kourier_model::{
    AccountId, AttachmentId, CmdId, ConvKey, ConversationId, MessageId, MsgIdScope, UserId,
};

pub(crate) use attachment::{forget_orphans, newest_download, pending_derives, resume_downloads};
pub(crate) use call::{DEAD_ORIGIN, is_call_line};
pub(crate) use clear::{LocalDelete, attachment_account, clear_conversation, delete_attachment};
pub(crate) use message::{MsgRow, preview};
pub(crate) use modifier::MAX_EMOJI_BYTES;

use crate::error::{CoreError, Result};
use crate::sql::{self, AccountInfo};

/// Kill point inside the ingest transaction, before `COMMIT` (§10.4): the envelope rolls back.
pub const FAILPOINT_BEFORE_COMMIT: &str = "core_ingest_before_commit";

/// What one ingest transaction changed. Turned into `UiEvent`s after the commit.
#[derive(Debug, Default)]
pub(crate) struct Changes {
    /// The account row changed (state, own ids, settings, caps).
    pub accounts: bool,
    /// `AccountReady` was applied.
    pub account_ready: bool,
    /// Conversation rows that changed.
    pub conversations: BTreeSet<ConversationId>,
    /// Conversations that are gone (`ConversationRemoved`).
    pub removed: BTreeSet<ConversationId>,
    /// Message rows that changed, per conversation.
    pub messages: BTreeMap<ConversationId, BTreeSet<MessageId>>,
    /// Conversations whose whole timeline must be reloaded (merges, identity rewrites).
    pub timelines: BTreeSet<ConversationId>,
    /// `merge_conversation(from, into)` runs.
    pub merged: Vec<(ConversationId, ConversationId)>,
    /// Contacts changed (names, avatars, trust).
    pub contacts: bool,
    /// New incoming messages that may be notified, in order.
    pub notify: Vec<NotifyCandidate>,
    /// One entry per new `catch_up` message (for the summary when `Syncing` ends, §4.5).
    pub catch_up: Vec<ConversationId>,
    /// Banners.
    pub notices: Vec<BackendNotice>,
    /// History import progress: conversation, progress, has_more.
    pub history: Vec<(ConversationId, Option<f32>, bool)>,
    /// New incoming messages by (conversation, author, device): they clear typing indicators.
    pub incoming: Vec<(ConversationId, UserId, Option<u32>)>,
    /// Blob files (relative to the account's blob directory) to delete after the commit.
    pub unlink: Vec<PathBuf>,
    /// Durable commands confirmed by the backend.
    pub commands_done: Vec<CmdId>,
    /// Downloads to send to the backend after the commit (§5.8).
    pub downloads: Vec<crate::attachments::DownloadJob>,
    /// Downloaded files to derive thumbnails and blurhashes from after the commit.
    pub derive: Vec<crate::attachments::DeriveJob>,
    /// Attachments whose files or state changed or that are gone: their media handles are
    /// forgotten (§8.6).
    pub media: BTreeSet<AttachmentId>,
    /// Attachments that are gone (their message was deleted): open playback files go too.
    pub media_gone: BTreeSet<AttachmentId>,
    /// A disappearing timer started or a message got an expiry: the sweeper looks again.
    pub expiring: bool,
    /// A call line rings (it has a deadline): the deadline sweep looks again
    /// ([`crate::calls`]).
    pub ringing: bool,
    /// Call lines the clock made missed calls late, or before the account's catch-up was
    /// done: not announced alone. They wait for the catch-up summary and count there only if
    /// they are still missed then ([`crate::calls::Watch`]).
    pub held_calls: Vec<NotifyCandidate>,
    /// Outbox rows went with something that was applied (a tombstone, an echo, a clear): what
    /// waited behind them is at the head of its conversation now, so the dispatcher looks
    /// again instead of sleeping until the timer of a row that is gone.
    pub outbox: bool,
    /// Conversations whose counters must be recomputed before the commit.
    recount: BTreeSet<ConversationId>,
}

impl Changes {
    /// Folds `other` (a later transaction of the same envelope) into `self`.
    pub(crate) fn absorb(&mut self, other: Changes) {
        self.accounts |= other.accounts;
        self.account_ready |= other.account_ready;
        self.conversations.extend(other.conversations);
        self.removed.extend(other.removed);
        for (c, ids) in other.messages {
            self.messages.entry(c).or_default().extend(ids);
        }
        self.timelines.extend(other.timelines);
        self.merged.extend(other.merged);
        self.contacts |= other.contacts;
        self.notify.extend(other.notify);
        self.catch_up.extend(other.catch_up);
        self.notices.extend(other.notices);
        self.history.extend(other.history);
        self.incoming.extend(other.incoming);
        self.unlink.extend(other.unlink);
        self.commands_done.extend(other.commands_done);
        self.downloads.extend(other.downloads);
        self.derive.extend(other.derive);
        self.media.extend(other.media);
        self.media_gone.extend(other.media_gone);
        self.expiring |= other.expiring;
        self.ringing |= other.ringing;
        self.held_calls.extend(other.held_calls);
        self.outbox |= other.outbox;
    }

    pub(crate) fn message(&mut self, conv: ConversationId, id: MessageId) {
        self.messages.entry(conv).or_default().insert(id);
        self.conversations.insert(conv);
        self.recount.insert(conv);
    }
}

/// A new incoming message the notification policy may show (§8.7); the focus check happens
/// after the commit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct NotifyCandidate {
    /// The conversation.
    pub conv: ConversationId,
    /// The message.
    pub message: MessageId,
    /// We are mentioned.
    pub mentions_me: bool,
}

/// Applies `events` of `account` in one `BEGIN IMMEDIATE` transaction. `scope` is the network's
/// message-id scope (§3.1); `now` is the core clock (pending-modifier expiry, bookkeeping).
pub(crate) fn apply_events(
    conn: &mut Connection,
    account: AccountId,
    scope: MsgIdScope,
    now: i64,
    events: Vec<BackendEvent>,
) -> Result<Changes> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let acc = sql::account_info(&tx, account)?.ok_or(CoreError::UnknownAccount(account))?;
    let mut apply = Apply {
        conn: &tx,
        acc,
        scope,
        now,
        ch: Changes::default(),
    };
    apply.purge_expired()?;
    for e in events {
        apply.event(e)?;
    }
    apply.finish()?;
    let changes = apply.ch;
    failpoint::hit(FAILPOINT_BEFORE_COMMIT);
    tx.commit()?;
    Ok(changes)
}

/// The state of one ingest transaction.
pub(crate) struct Apply<'c> {
    pub(crate) conn: &'c Connection,
    pub(crate) acc: AccountInfo,
    pub(crate) scope: MsgIdScope,
    pub(crate) now: i64,
    pub(crate) ch: Changes,
}

impl<'c> Apply<'c> {
    /// The ingest state of `account` inside the transaction `conn` (for core-local writes that
    /// reuse ingest's rules: outbox, downloads).
    pub(crate) fn begin(
        conn: &'c Connection,
        account: AccountId,
        scope: MsgIdScope,
        now: i64,
    ) -> Result<Self> {
        let acc = sql::account_info(conn, account)?.ok_or(CoreError::UnknownAccount(account))?;
        Ok(Self {
            conn,
            acc,
            scope,
            now,
            ch: Changes::default(),
        })
    }
}

impl Apply<'_> {
    fn event(&mut self, e: BackendEvent) -> Result<()> {
        match e {
            // Ephemeral events never reach a transaction (the supervisor handles them).
            BackendEvent::Login(_)
            | BackendEvent::Typing { .. }
            | BackendEvent::AttachmentProgress { .. }
            | BackendEvent::DownloadProgress { .. }
            | BackendEvent::TransmitProgress { .. }
            | BackendEvent::HistoryTransfer(_) => Ok(()),
            BackendEvent::AccountReady {
                own_ids,
                device,
                display_name,
                number,
            } => self.account_ready(
                own_ids,
                device,
                display_name.map(|n| n.into_inner()),
                number.map(|n| n.into_inner()),
            ),
            BackendEvent::AccountSettings(p) => self.account_settings(p),
            BackendEvent::AccountCaps(c) => self.account_caps(&c),
            BackendEvent::ContactUpsert(p) => self.contact_upsert(p),
            BackendEvent::IdentityMerged { from, into } => self.identity_merged(&from, &into),
            BackendEvent::ConversationUpsert(p) => self.conversation_upsert(p),
            BackendEvent::PinnedConversations(keys) => self.pinned(&keys),
            BackendEvent::ConversationRemoved { conv } => self.conversation_removed(&conv),
            BackendEvent::ConversationCleared {
                conv,
                up_to,
                remove,
                ..
            } => self.conversation_cleared(&conv, &up_to, remove),
            BackendEvent::Message(m) => self.message(m, false),
            BackendEvent::HistoryBatch {
                conv,
                messages,
                has_more,
                progress,
            } => {
                for m in messages {
                    self.message(m, true)?;
                }
                let key = self.canon_conv(&conv)?;
                if let Some(id) = sql::conversation_id(self.conn, self.acc.id, &key)? {
                    self.ch.history.push((id, progress, has_more));
                }
                Ok(())
            }
            BackendEvent::Receipts {
                targets,
                by,
                kind,
                at,
            } => {
                for t in targets {
                    self.modifier(BackendEvent::Receipts {
                        targets: vec![t],
                        by: by.clone(),
                        kind,
                        at,
                    })?;
                }
                Ok(())
            }
            e @ (BackendEvent::Edited { .. }
            | BackendEvent::Deleted { .. }
            | BackendEvent::Reaction { .. }
            | BackendEvent::AttachmentDeleted { .. }
            | BackendEvent::ReadMarker { .. }
            | BackendEvent::OwnSendState { .. }) => self.modifier(e),
            BackendEvent::CommandPersisted { cmd } => {
                self.conn
                    .prepare_cached(
                        "DELETE FROM pending_commands WHERE id = ?1 AND account_id = ?2",
                    )?
                    .execute((cmd.0, self.acc.id.0))?;
                self.ch.commands_done.push(cmd);
                Ok(())
            }
            BackendEvent::AttachmentDone {
                attachment,
                request,
                result,
            } => self.attachment_done(attachment, request, result),
            BackendEvent::Notice(n) => {
                self.ch.notices.push(n);
                Ok(())
            }
        }
    }

    /// Recomputes counters of touched conversations (call once before the commit).
    pub(crate) fn finish(&mut self) -> Result<()> {
        let convs = std::mem::take(&mut self.ch.recount);
        for c in convs {
            if self.ch.removed.contains(&c) {
                continue;
            }
            self.recount(c)?;
            self.ch.conversations.insert(c);
        }
        Ok(())
    }

    /// Drops pending modifiers past their expiry (§4.9: 2 or 7 days).
    fn purge_expired(&self) -> Result<()> {
        self.conn
            .prepare_cached(
                "DELETE FROM pending_modifiers WHERE account_id = ?1 AND expires_at < ?2",
            )?
            .execute((self.acc.id.0, self.now))?;
        Ok(())
    }

    /// `unread`, `mentions` and `last_activity` from the rows (§3.3 step 3).
    pub(crate) fn recount(&self, conv: ConversationId) -> Result<()> {
        recount(self.conn, conv)
    }

    // ---------- identity helpers ----------

    /// The canonical id of `user` (through `identity_alias`).
    pub(crate) fn resolve_user(&self, user: &UserId) -> Result<UserId> {
        let canonical: Option<String> = self
            .conn
            .prepare_cached(
                "SELECT user_id FROM identity_alias WHERE account_id = ?1 AND alias = ?2",
            )?
            .query_row((self.acc.id.0, user.as_str()), |r| r.get(0))
            .optional()?;
        match canonical {
            Some(c) => sql::user(&c),
            None => Ok(user.clone()),
        }
    }

    /// Whether `user` is one of our own ids.
    pub(crate) fn is_own(&self, user: &UserId) -> bool {
        self.acc.own_ids.contains(user)
    }

    /// The stored author column of `by`: `''` for own (`None` or one of our ids), else the
    /// canonical user id.
    pub(crate) fn author_col(&self, by: Option<&UserId>) -> Result<String> {
        match by {
            None => Ok(String::new()),
            Some(u) if self.is_own(u) => Ok(String::new()),
            Some(u) => {
                let c = self.resolve_user(u)?;
                Ok(if self.is_own(&c) {
                    String::new()
                } else {
                    c.as_str().to_owned()
                })
            }
        }
    }

    /// The canonical conversation key (direct peers through `identity_alias`).
    pub(crate) fn canon_conv(&self, key: &ConvKey) -> Result<ConvKey> {
        Ok(match key {
            ConvKey::Direct(u) => ConvKey::Direct(self.resolve_user(u)?),
            other => other.clone(),
        })
    }

    /// The conversation with `key` (canonical), created when missing.
    pub(crate) fn ensure_conversation(&mut self, key: &ConvKey) -> Result<ConversationId> {
        if let Some(id) = sql::conversation_id(self.conn, self.acc.id, key)? {
            return Ok(id);
        }
        let kind = sql::enum_name(&key.kind())?;
        let is_self = key.direct_peer().is_some_and(|p| self.is_own(p));
        self.conn
            .prepare_cached(
                "INSERT INTO conversations(account_id, conv_key, kind, is_self) VALUES (?1, ?2, ?3, ?4)",
            )?
            .execute((self.acc.id.0, key.encode(), kind, is_self))?;
        let id = ConversationId(self.conn.last_insert_rowid());
        self.ch.conversations.insert(id);
        Ok(id)
    }
}

/// The statement of [`recount`].
const RECOUNT: &str = "UPDATE conversations SET
           unread = unread_hint + (SELECT count(*) FROM messages
             WHERE conversation_id = ?1 AND read_state = 'Unread'
               AND deleted IS NULL AND expiry_hidden = 0),
           mentions = (SELECT count(*) FROM messages
             WHERE conversation_id = ?1 AND read_state = 'Unread'
               AND deleted IS NULL AND mentions_me = 1 AND expiry_hidden = 0),
           last_activity = (SELECT max(sort_key) FROM messages
             WHERE conversation_id = ?1 AND (deleted IS NULL OR deleted <> 'ForMe')
               AND expiry_hidden = 0)
         WHERE id = ?1";

/// Marks every unread message of a conversation (`?1`) whose `sent_at` is at or before a
/// watermark (`?2`) read, and returns their ids: a read marker of another device
/// (`modifier.rs` `read_marker`) and the watermark of two merged chats (`merge.rs`). It
/// leaves `deleted` alone on purpose (a tombstone keeps its read state, or a marker and a
/// delete would end differently in the two orders), so the partial index of the unread
/// rows, `read_state = 'Unread'` alone, is the one that serves it: a statement that also
/// asked for `deleted IS NULL` (as the first form of that index did) walked the whole
/// conversation for every marker. The plan is held by `ingest::tests`.
pub(crate) const READ_UP_TO: &str = "UPDATE messages SET read_state = 'Read'
           WHERE conversation_id = ?1 AND read_state = 'Unread' AND sent_at <= ?2
           RETURNING id";

/// `unread`, `mentions` and `last_activity` of `conv` from its rows (§3.3 step 3).
///
/// THE ROWS ARE THE TRUTH about what is unread: every row whose read state is `Unread`
/// counts, a live one and one of a history batch alike (a backend says `read: false` for a
/// history row only when it KNOWS the message is unread). `unread_hint` adds the unread
/// messages a backend has no rows for, and nothing else. So reading the chat here, a read
/// marker of another device, a delete and a timer that runs out all lower the count by the
/// row they touch, and a row that is delivered twice counts once.
///
/// A message hidden because its time has come on an unconfirmed clock (`expiry_hidden`,
/// [`crate::expiry`]) counts for none of them: the list shows the conversation as if the
/// message were gone.
pub(crate) fn recount(conn: &Connection, conv: ConversationId) -> Result<()> {
    conn.prepare_cached(RECOUNT)?.execute([conv.0])?;
    Ok(())
}

/// `n` as a lowercase hex string (dedup keys).
pub(crate) fn hex64(n: u64) -> String {
    format!("{n:016x}")
}

#[cfg(test)]
mod tests {
    use kourier_model::{MsgIdScope, NetworkKind, UserId};

    /// `core.db` has no planner statistics. What every ingest transaction, every read marker
    /// of another device and every read of a chat count or mark the unread rows with must go
    /// through the partial index of the unread rows, never through all rows of the
    /// conversation: [`super::recount`] runs once per touched conversation and transaction,
    /// also for every batch of a history import into a chat of tens of thousands of rows, and
    /// Signal sends one read marker per message the phone reads. Schema 12 took `history = 0`
    /// out of the index (a statement that counts unread history rows could not use it) and
    /// `deleted IS NULL` too (the statement of [`super::READ_UP_TO`] cannot say it, and
    /// walked the whole conversation for every marker).
    #[test]
    fn the_unread_counts_use_the_index_of_unread_rows() {
        let dir = tempfile::tempdir().expect("tempdir");
        let h = crate::testing::IngestHarness::open(
            &dir.path().join("core.db"),
            &kourier_db::DbKey::from([3u8; 32]),
            NetworkKind::Fake,
            MsgIdScope::AccountAuthor,
            &[UserId::parse("aci:me").expect("user id")],
        )
        .expect("harness");
        let definition: String = h
            .connection()
            .query_row(
                "SELECT sql FROM sqlite_master WHERE name = 'messages_unread'",
                [],
                |r| r.get(0),
            )
            .expect("the index");
        let condition = definition
            .split_once("WHERE")
            .map(|(_, condition)| condition.split_whitespace().collect::<Vec<_>>().join(" "))
            .expect("a partial index");
        assert_eq!(
            condition, "read_state = 'Unread'",
            "the index asks for nothing but an unread row: {definition}"
        );
        let plan = |sql: &str| -> Vec<String> {
            h.connection()
                .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
                .expect("prepare")
                .query_map([], |r| r.get::<_, String>(3))
                .expect("plan")
                .collect::<kourier_db::rusqlite::Result<_>>()
                .expect("rows")
        };
        // The two counts of `recount` (its third subquery is the newest row: another index).
        let steps = plan(&super::RECOUNT.replace("?1", "1"));
        let counting: Vec<&String> = steps
            .iter()
            .filter(|s| s.contains("messages_unread"))
            .collect();
        assert_eq!(counting.len(), 2, "{steps:#?}");
        assert!(
            steps
                .iter()
                .filter(|s| s.contains("messages"))
                .all(|s| s.contains("USING") && !s.contains("SCAN messages")),
            "no walk through the table: {steps:#?}"
        );
        // The marking by a watermark (a read marker of another device, a merge of two
        // chats): the statement is not pinned, so the planner's own choice is what is held
        // (it prefers a partial index the statement implies; the chat's timeline index
        // would walk every row of the conversation).
        let steps = plan(&super::READ_UP_TO.replace("?1", "1").replace("?2", "5"));
        assert!(
            steps
                .iter()
                .any(|s| s.contains("USING INDEX messages_unread")),
            "the watermark update goes through the unread rows: {steps:#?}"
        );
        assert!(
            !steps.iter().any(|s| s.contains("messages_timeline")),
            "and not through the whole conversation: {steps:#?}"
        );
        // The list of what a read of the chat marks (`local::mark_read`). That statement
        // names the index (`INDEXED BY`, or its `ORDER BY` would prefer the timeline index),
        // so its prepare fails when the index cannot serve it; the plan is shown for the
        // record.
        let steps = plan(&crate::local::UNREAD_ROWS.replace("?1", "1"));
        assert!(
            steps.iter().all(|s| !s.contains("SCAN messages")),
            "{steps:#?}"
        );
    }
}
