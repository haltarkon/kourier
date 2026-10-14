//! `Message` and `HistoryBatch` ingest (§4.9 rows 1-2), sort keys (§3.9), placeholders, echo
//! dedup of own messages (§4.8 step 6) and the derived per-message state (revisions, aggregated
//! outgoing state).

use std::collections::BTreeMap;

use kourier_backend_api::{HistoryEdit, IncomingMessage, MAX_HISTORY_EDITS};
use kourier_db::rusqlite::{OptionalExtension, Row, params};
use kourier_model::{
    Body, ConvKind, ConversationId, DisappearKind, MessageId, MsgIdScope, Origin, OutgoingState,
    Placeholder, ReceiptKind, RecipientState, UserId,
};

use super::{Apply, NotifyCandidate};
use crate::error::Result;
use crate::sql;

/// The columns of a message the modifier rules need.
#[derive(Clone, Debug)]
pub(crate) struct MsgRow {
    pub id: MessageId,
    pub conv: ConversationId,
    pub conv_key: String,
    pub net_author: String,
    pub net_local: String,
    pub sent_at: i64,
    pub sort_key: i64,
    pub deleted: Option<String>,
    pub body_kind: String,
    pub revision: u16,
    pub read_state: Option<String>,
    /// The server's time for the message, when the network gave one.
    pub server_at: Option<i64>,
}

impl MsgRow {
    pub(crate) const SELECT: &'static str = "SELECT m.id, m.conversation_id, c.conv_key,
        m.net_author, m.net_local, m.sent_at, m.sort_key, m.deleted, m.body_kind,
        m.revision, m.read_state, m.server_at
        FROM messages m JOIN conversations c ON c.id = m.conversation_id";

    pub(crate) fn from_row(r: &Row<'_>) -> kourier_db::rusqlite::Result<Self> {
        Ok(Self {
            id: MessageId(r.get(0)?),
            conv: ConversationId(r.get(1)?),
            conv_key: r.get(2)?,
            net_author: r.get(3)?,
            net_local: r.get(4)?,
            sent_at: r.get(5)?,
            sort_key: r.get(6)?,
            deleted: r.get(7)?,
            body_kind: r.get(8)?,
            revision: u16::try_from(r.get::<_, i64>(9)?.max(0)).unwrap_or(u16::MAX),
            read_state: r.get(10)?,
            server_at: r.get(11)?,
        })
    }

    /// Whether our account wrote it.
    pub(crate) fn is_own(&self) -> bool {
        self.net_author.is_empty()
    }

    /// The instant a receive window of the message is measured from (§3.8): the server's time
    /// for it, else the sender's. Signal Desktop measures the same way
    /// (`SD/ts/util/getMessageAge.std.ts`: `serverTimestamp ?? sent_at`), so a sender cannot
    /// move the window with its own clock where the server dated the message.
    pub(crate) fn dated_at(&self) -> i64 {
        self.server_at.unwrap_or(self.sent_at)
    }
}

/// Conversation fields that the message rules read.
pub(super) struct ConvState {
    kind: String,
    pub(super) blocked: bool,
    muted_until: Option<i64>,
    local_muted_until: Option<i64>,
    read_sent_at: Option<i64>,
}

/// Whether a body carries attachment rows.
fn has_attachments(body: &Body) -> bool {
    matches!(body, Body::Media { .. } | Body::Sticker(_))
}

/// Body kinds that a later real message replaces in place (§3.9 placeholder replacement).
fn is_replaceable_placeholder(kind: &str, body: &str) -> bool {
    kind == "Placeholder"
        && sql::from_json::<Body>(body)
            .is_ok_and(|b| matches!(b, Body::Placeholder(Placeholder::Undecryptable { .. })))
}

/// Plain text for the conversation list, at most this many bytes.
const PREVIEW_MAX: usize = 256;

/// The preview text of a body (stored in `messages.preview`).
pub(crate) fn preview(body: &Body) -> Option<String> {
    let text = match body {
        Body::Text(t) => Some(t.body.as_str()),
        Body::Media { caption, .. } => caption.as_ref().map(|t| t.body.as_str()),
        Body::Sticker(a) => a.caption.as_ref().map(|t| t.body.as_str()),
        Body::Contact(c) => c.name.as_deref(),
        Body::Location(l) => l.name.as_deref(),
        Body::Poll(p) => Some(p.question.as_str()),
        Body::Unsupported { summary, .. } => summary.as_deref(),
        Body::System(_) | Body::Placeholder(_) => None,
    }?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut end = trimmed.len().min(PREVIEW_MAX);
    while !trimmed.is_char_boundary(end) {
        end -= 1;
    }
    Some(trimmed[..end].to_owned())
}

/// The kind name of a stored body (`messages.body_kind`).
fn kind_of(body: &Body) -> &'static str {
    body.kind_name()
}

/// History rows sort by `sent_at` (§3.9); the low bits are a stable function of the local id
/// (not the author, which an identity merge may rewrite), so imports interleave identically in
/// any order.
fn history_sort_key(sent_at: i64, local: &str) -> i64 {
    let h = kourier_model::stable_hash(local.as_bytes());
    // `sent_at` is the sender's claim: saturate, never overflow (a panic here would fail the
    // envelope on every redelivery).
    sent_at
        .saturating_mul(1024)
        .saturating_add(i64::try_from(h & 1023).unwrap_or(0))
}

impl Apply<'_> {
    /// Loads one message.
    pub(crate) fn msg_row(&self, id: MessageId) -> Result<Option<MsgRow>> {
        let sql = format!("{} WHERE m.id = ?1", MsgRow::SELECT);
        Ok(self
            .conn
            .prepare_cached(&sql)?
            .query_row([id.0], MsgRow::from_row)
            .optional()?)
    }

    /// The message with key `(author, local)` under the network scope (§3.1). A call line is
    /// keyed by its conversation on every network ([`super::call`]): under `AccountAuthor`
    /// scope an own key that names no message names the call line of `conv` with that id,
    /// when there is one.
    pub(crate) fn by_key(
        &self,
        conv: ConversationId,
        author: &str,
        local: &str,
    ) -> Result<Option<MsgRow>> {
        let id: Option<i64> = match self.scope {
            MsgIdScope::AccountAuthor => {
                let id: Option<i64> = self
                    .conn
                    .prepare_cached(
                        "SELECT id FROM messages WHERE account_id = ?1 AND net_author = ?2
                           AND net_local = ?3 AND net_scope = 0",
                    )?
                    .query_row((self.acc.id.0, author, local), |r| r.get(0))
                    .optional()?;
                if id.is_none() && author.is_empty() {
                    return self.call_row(conv, local);
                }
                id
            }
            MsgIdScope::Conversation => self
                .conn
                .prepare_cached(
                    "SELECT id FROM messages WHERE conversation_id = ?1 AND net_local = ?2
                       AND net_scope = 1",
                )?
                .query_row((conv.0, local), |r| r.get(0))
                .optional()?,
        };
        match id {
            Some(id) => self.msg_row(MessageId(id)),
            None => Ok(None),
        }
    }

    pub(super) fn conv_state(&self, conv: ConversationId) -> Result<ConvState> {
        Ok(self
            .conn
            .prepare_cached(
                "SELECT kind, blocked, muted_until, local_muted_until, read_sent_at
                 FROM conversations WHERE id = ?1",
            )?
            .query_row([conv.0], |r| {
                Ok(ConvState {
                    kind: r.get(0)?,
                    blocked: r.get(1)?,
                    muted_until: r.get(2)?,
                    local_muted_until: r.get(3)?,
                    read_sent_at: r.get(4)?,
                })
            })?)
    }

    /// Where a new row of `conv` sorts (§3.9): an imported one by its `sent_at`, a live one
    /// behind everything that is there, by its arrival.
    pub(super) fn sort_key_of(
        &self,
        conv: ConversationId,
        m: &IncomingMessage,
        history: bool,
    ) -> Result<i64> {
        if history {
            return Ok(history_sort_key(m.sent_at, &m.local));
        }
        let last: Option<i64> = self
            .conn
            .prepare_cached("SELECT max(sort_key) FROM messages WHERE conversation_id = ?1")?
            .query_row([conv.0], |r| r.get(0))?;
        let tie = i64::try_from(m.arrival.unwrap_or(0) & 1023).unwrap_or(0);
        let candidate = m.received_at.saturating_mul(1024).saturating_add(tie);
        Ok(last.map_or(candidate, |l| candidate.max(l.saturating_add(1))))
    }

    /// Whether `conv` exists and is hidden by a delete (not listed).
    fn in_deleted_chat(&self, conv: &kourier_model::ConvKey) -> Result<bool> {
        let key = self.canon_conv(conv)?;
        let hidden: Option<bool> = self
            .conn
            .prepare_cached(
                "SELECT hidden FROM conversations WHERE account_id = ?1 AND conv_key = ?2",
            )?
            .query_row((self.acc.id.0, key.encode()), |r| r.get(0))
            .optional()?;
        Ok(hidden == Some(true))
    }

    /// Ingests one live (`history = false`) or imported message.
    pub(crate) fn message(&mut self, mut m: IncomingMessage, history: bool) -> Result<()> {
        // An imported row of a chat that was deleted meanwhile (a history is delivered for a
        // while, and the phone can delete a chat as well) is dropped: it must neither bring
        // the chat back with the old messages it still holds nor fail the delivery. A LIVE
        // message of such a chat does bring it back ([`Self::unhide`]); history is not news.
        if history && self.in_deleted_chat(&m.conv)? {
            tracing::debug!(
                account = self.acc.id.0,
                "an imported message of a deleted chat was dropped"
            );
            return Ok(());
        }
        // A signal of a call: its line is made or changed, never duplicated ([`super::call`]).
        if let Some(line) = m.body.call_line().cloned() {
            return self.call_signal(m, line, history);
        }
        m.body.sanitize_ranges();
        let key = self.canon_conv(&m.conv)?;
        // A remote origin naming one of our ids is our own message (contract §10.2).
        let origin = match m.origin.clone() {
            Origin::Remote { author, device } => {
                let a = self.resolve_user(&author)?;
                if self.is_own(&a) {
                    Origin::OwnOtherDevice { device }
                } else {
                    Origin::Remote { author: a, device }
                }
            }
            other => other,
        };
        m.origin = origin.clone();
        let author = origin
            .author()
            .map_or(String::new(), |a| a.as_str().to_owned());
        // Look the key up before creating the conversation: a duplicate that is dropped (the
        // same Signal author + timestamp in another conversation) must not leave an empty chat.
        let found = match (
            sql::conversation_id(self.conn, self.acc.id, &key)?,
            self.scope,
        ) {
            (Some(c), _) => self.by_key(c, &author, &m.local)?,
            // Account-wide keys do not need the conversation.
            (None, MsgIdScope::AccountAuthor) => {
                self.by_key(ConversationId(0), &author, &m.local)?
            }
            (None, MsgIdScope::Conversation) => None,
        };
        if let Some(existing) = found {
            return self.existing(existing, &author, m, history);
        }
        // A copy of a message whose timer ran out here (its row is gone): it stays gone, and
        // leaves no empty chat behind.
        if crate::expiry::remembered(
            self.conn,
            self.acc.id,
            self.scope,
            &key.encode(),
            &author,
            &m.local,
            self.now,
        )? {
            tracing::debug!(
                account = self.acc.id.0,
                "a copy of an expired message was dropped"
            );
            return Ok(());
        }
        let conv = self.ensure_conversation(&key)?;

        let cs = self.conv_state(conv)?;
        let sort_key = self.sort_key_of(conv, &m, history)?;
        let remote = matches!(origin, Origin::Remote { .. });
        // System notices (group updates, safety-number changes) are never unread.
        let read_state = if remote && !matches!(m.body, Body::System(_)) {
            let read_elsewhere = cs.read_sent_at.is_some_and(|w| m.sent_at <= w);
            Some(if m.read || read_elsewhere {
                "Read"
            } else {
                "Unread"
            })
        } else {
            None
        };
        // A notice of our own doing (a timer we changed) is not a message that was sent: it
        // has no outgoing state.
        let floor = if remote || matches!(m.body, Body::System(_)) {
            None
        } else {
            Some(m.initial_out_state.clone().unwrap_or(OutgoingState::Sent {
                server_at: m.server_at,
            }))
        };
        let linked: Option<i64> = match m.linked_local {
            Some(l) => self
                .conn
                .prepare_cached(
                    "SELECT id FROM messages WHERE id = ?1 AND account_id = ?2 AND net_author = ''",
                )?
                .query_row((l.0, self.acc.id.0), |r| r.get(0))
                .optional()?,
            None => None,
        };
        let origin_name = match origin {
            Origin::Remote { .. } => "Remote",
            Origin::OwnThisDevice => "OwnThisDevice",
            Origin::OwnOtherDevice { .. } => "OwnOtherDevice",
        };
        // When the timer starts (§3.5 `Expiry`): as the backend says; else at the send for
        // `AfterSend` and for our own messages, at reading for incoming `AfterRead` ones (now,
        // when the message is already read elsewhere).
        //
        // Two rows have no such instant yet or ever. An own message that was NOT sent (its
        // state says queued, sending or failed: a history import of a message that was
        // still pending on the phone) keeps its timer without a start, or it would be
        // erased `timer` after a send that never happened; its echo starts it
        // ([`Self::existing`]). And a notice somebody else caused is never unread, so no
        // reading would ever start its `AfterRead` timer: it starts when the notice is
        // stored.
        if let Some(e) = m.expiry.as_mut()
            && e.started_at.is_none()
        {
            let read = read_state == Some("Read");
            let unsent = floor.as_ref().is_some_and(|state| !was_sent(state));
            let notice = remote && read_state.is_none();
            e.started_at = match e.kind {
                _ if unsent => None,
                DisappearKind::AfterSend => Some(m.sent_at),
                DisappearKind::AfterRead if !remote => Some(m.sent_at),
                DisappearKind::AfterRead if read || notice => Some(self.now),
                DisappearKind::AfterRead => None,
            };
        }
        if m.expiry.and_then(|e| e.expires_at()).is_some() {
            self.ch.expiring = true;
        }
        let expiry_kind = m.expiry.map(|e| sql::enum_name(&e.kind)).transpose()?;
        let expiry_ms = m
            .expiry
            .map(|e| i64::try_from(e.timer.as_millis()).unwrap_or(i64::MAX));
        let floor_json = floor.as_ref().map(sql::to_json).transpose()?;
        self.conn
            .prepare_cached(
                "INSERT INTO messages(account_id, conversation_id, net_scope, net_author, net_local,
                   origin, device, linked_message_id, sort_key, sent_at, server_at, received_at,
                   arrival, body_kind, body, preview, reply_to, out_floor, out_state, read_state,
                   expiry_kind, expiry_timer_ms, expiry_started_at, expires_at, view_once, history,
                   silent, mentions_me, meta, raw)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17,
                   ?18, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29)",
            )?
            .execute(params![
                self.acc.id.0,
                conv.0,
                self.scope.db_code(),
                author,
                m.local.as_ref(),
                origin_name,
                origin.device(),
                linked,
                sort_key,
                m.sent_at,
                m.server_at,
                m.received_at,
                m.arrival.map(sql::i64_of),
                kind_of(&m.body),
                sql::to_json(&m.body)?,
                preview(&m.body),
                m.reply_to.as_ref().map(sql::to_json).transpose()?,
                floor_json,
                read_state,
                expiry_kind,
                expiry_ms,
                m.expiry.and_then(|e| e.started_at),
                m.expiry.and_then(|e| e.expires_at()),
                m.view_once,
                history,
                m.silent || history,
                m.mentions_me,
                m.meta.as_ref().map(sql::blob_to_sql),
                m.raw.as_ref().map(sql::blob_to_sql),
            ])?;
        let id = MessageId(self.conn.last_insert_rowid());
        if has_attachments(&m.body) {
            self.insert_attachments(id, &m.body)?;
            self.plan_downloads(id, conv, history)?;
        }
        // A chat deleted on another device comes back with its next message.
        self.unhide(conv)?;
        self.ch.message(conv, id);
        if let Origin::Remote { author, device } = &origin {
            self.ch.incoming.push((conv, author.clone(), *device));
        }

        // An imported message that was edited before the import: its versions' keys and the
        // "edited" mark (only for a NEW row of a history batch; a row that is there ignores
        // the field, [`Self::existing`]).
        let versions = if history && !matches!(m.body, Body::System(_)) {
            self.history_edits(id, &author, &m)?
        } else {
            Vec::new()
        };
        let row = self.msg_row(id)?;
        if let Some(row) = &row {
            for r in &m.reactions {
                self.reaction(row, r.by.as_ref(), &r.emoji, false, r.at)?;
            }
            // Modifiers that waited for this message, in arrival order: under its own id
            // and under the id of each later version.
            self.apply_pending(row, &row.net_local.clone())?;
            for local in &versions {
                self.apply_pending(row, local)?;
            }
        }
        if !remote {
            self.recompute_out(id)?;
        }

        // Notification policy (§8.7): after the pending modifiers (a waiting read marker or
        // delete suppresses it).
        // Undecryptable placeholders notify only once their resend replaces them.
        let placeholder = matches!(m.body, Body::Placeholder(_));
        if remote && !history && !m.silent && !placeholder {
            let fresh = self.msg_row(id)?;
            let unread = fresh
                .as_ref()
                .is_some_and(|r| r.read_state.as_deref() == Some("Unread") && r.deleted.is_none());
            if unread && !cs.blocked {
                if m.catch_up {
                    self.ch.catch_up.push(conv);
                } else if self.may_notify(&cs, m.mentions_me) {
                    self.ch.notify.push(NotifyCandidate {
                        conv,
                        message: id,
                        mentions_me: m.mentions_me,
                    });
                }
            }
        }
        Ok(())
    }

    /// Mute rules (§8.7): muted 1:1 chats never notify, muted groups only for mentions.
    pub(super) fn may_notify(&self, cs: &ConvState, mentions_me: bool) -> bool {
        if !self.acc.local_notifications {
            return false;
        }
        let now = self.now;
        let muted = cs.muted_until.is_some_and(|t| t > now)
            || cs.local_muted_until.is_some_and(|t| t > now)
            || self.acc.local_muted_until.is_some_and(|t| t > now);
        if !muted {
            return true;
        }
        let group = cs.kind != sql::enum_name(&ConvKind::Direct).unwrap_or_default();
        group && mentions_me
    }

    /// A message whose key exists already: placeholder replacement, echo merge or duplicate.
    fn existing(
        &mut self,
        ex: MsgRow,
        author: &str,
        m: IncomingMessage,
        history: bool,
    ) -> Result<()> {
        if self.scope == MsgIdScope::Conversation && ex.net_author != author {
            // Same (conversation, local) from another author: a backend bug or a collision.
            tracing::warn!(
                message = ex.id.0,
                "message key collision with another author; ignored"
            );
            return Ok(());
        }
        if ex.deleted.is_some() {
            return Ok(());
        }
        // A message that is no call line never merges into one (it is not the echo of
        // anything we sent, whatever its id says).
        if ex.is_own() && ex.body_kind == "System" && super::is_call_line(self.conn, ex.id)? {
            return Ok(());
        }
        let body: String = self
            .conn
            .prepare_cached("SELECT body FROM messages WHERE id = ?1")?
            .query_row([ex.id.0], |r| r.get(0))?;
        let incoming_is_placeholder = matches!(m.body, Body::Placeholder(_));
        if is_replaceable_placeholder(&ex.body_kind, &body) && !incoming_is_placeholder {
            // The resend of an undecryptable message: keep the row and its sort_key.
            self.conn
                .prepare_cached(
                    "UPDATE messages SET body_kind = ?2, body = ?3, preview = ?4, reply_to = ?5,
                       view_once = ?6, mentions_me = ?7, meta = ?8, raw = ?9 WHERE id = ?1",
                )?
                .execute(params![
                    ex.id.0,
                    kind_of(&m.body),
                    sql::to_json(&m.body)?,
                    preview(&m.body),
                    m.reply_to.as_ref().map(sql::to_json).transpose()?,
                    m.view_once,
                    m.mentions_me,
                    m.meta.as_ref().map(sql::blob_to_sql),
                    m.raw.as_ref().map(sql::blob_to_sql),
                ])?;
            self.delete_attachments(ex.id)?;
            if has_attachments(&m.body) {
                self.insert_attachments(ex.id, &m.body)?;
                self.plan_downloads(ex.id, ex.conv, history)?;
            }
            // A history batch that fills a placeholder brings the message as it stands, and
            // so its versions: the row takes the newest text, so it takes the "edited" mark
            // and the keys of the versions too (a later modifier that names the newest
            // version finds it). A live message has none.
            if history && !matches!(m.body, Body::System(_)) {
                let versions = self.history_edits(ex.id, author, &m)?;
                if let Some(row) = self.msg_row(ex.id)?.filter(|_| !versions.is_empty()) {
                    for local in &versions {
                        self.apply_pending(&row, local)?;
                    }
                }
            }
            self.ch.message(ex.conv, ex.id);
            // Not for a row the sweeper hid until its clock is confirmed: it is gone for
            // the user and the desktop ([`crate::expiry`]).
            let hidden: bool = self
                .conn
                .prepare_cached("SELECT expiry_hidden FROM messages WHERE id = ?1")?
                .query_row([ex.id.0], |r| r.get(0))?;
            let notify = !history && !m.silent && !m.catch_up && !ex.is_own() && !hidden;
            if notify && ex.read_state.as_deref() == Some("Unread") {
                let cs = self.conv_state(ex.conv)?;
                if !cs.blocked && self.may_notify(&cs, m.mentions_me) {
                    self.ch.notify.push(NotifyCandidate {
                        conv: ex.conv,
                        message: ex.id,
                        mentions_me: m.mentions_me,
                    });
                }
            }
            return Ok(());
        }
        if ex.is_own() && m.origin.is_own() {
            // Echo or second copy of an own message (§4.8 step 6): states merge forward (so any
            // arrival order of two copies converges); the outbox item is done.
            let floor: Option<String> = self
                .conn
                .prepare_cached("SELECT out_floor FROM messages WHERE id = ?1")?
                .query_row([ex.id.0], |r| r.get(0))?;
            let floor: Option<OutgoingState> = sql::from_json_opt(floor)?;
            let incoming = m.initial_out_state.clone().unwrap_or(OutgoingState::Sent {
                server_at: m.server_at,
            });
            let merged = merge_out(floor.clone(), Some(incoming.clone()));
            if merged != floor {
                self.conn
                    .prepare_cached("UPDATE messages SET out_floor = ?2 WHERE id = ?1")?
                    .execute((ex.id.0, merged.as_ref().map(sql::to_json).transpose()?))?;
                self.ch.message(ex.conv, ex.id);
            }
            self.conn
                .prepare_cached(
                    "UPDATE messages SET server_at = CASE WHEN server_at IS NULL THEN ?2
                       WHEN ?2 IS NULL THEN server_at ELSE min(server_at, ?2) END WHERE id = ?1",
                )?
                .execute((ex.id.0, m.server_at))?;
            let done = self
                .conn
                .prepare_cached("DELETE FROM outbox WHERE message_id = ?1 AND kind = 'Message'")?
                .execute([ex.id.0])?;
            self.ch.outbox |= done > 0;
            // An echo of a queued message means it was sent: its timer starts. Not a copy
            // that itself says "not sent" (the same row of a history import delivered
            // twice, of a message still pending on the phone).
            if was_sent(&incoming) {
                self.start_send_timer(ex.id)?;
            }
            self.recompute_out(ex.id)?;
        } else if !ex.is_own() && m.read {
            // A copy that says "already read elsewhere" (history imports).
            self.raise_read_state(&ex, kourier_model::ReadState::Read)?;
        }
        for r in &m.reactions {
            self.reaction(&ex, r.by.as_ref(), &r.emoji, false, r.at)?;
        }
        // Anything else of a duplicate (including the same Signal author + timestamp in another
        // conversation) is ignored.
        Ok(())
    }

    /// The versions of a NEW history row that was edited before it was imported
    /// ([`IncomingMessage::edits`]): one `message_revisions` row under the original's key
    /// (no body: the earlier texts are not imported) and one per later version, the newest
    /// with the row's body; then the "edited" mark ([`Self::reindex_revisions`]). The rows
    /// are what makes a later reaction, receipt, quote or edit that names the newest
    /// version's id find the message (`resolve_target`), and what a live edit of it counts
    /// its revisions from. Read: the newest [`MAX_HISTORY_EDITS`], one per id, none under
    /// the original's own id. Returns the ids of the versions that were stored.
    fn history_edits(
        &mut self,
        id: MessageId,
        author: &str,
        m: &IncomingMessage,
    ) -> Result<Vec<std::sync::Arc<str>>> {
        let mut edits: Vec<&HistoryEdit> = m.edits.iter().filter(|e| e.local != m.local).collect();
        if edits.is_empty() {
            return Ok(Vec::new());
        }
        // The order `reindex_revisions` numbers them in: the last is the newest version.
        edits.sort_by(|a, b| (a.at, &a.local).cmp(&(b.at, &b.local)));
        let mut seen = std::collections::BTreeSet::new();
        let mut kept: Vec<&HistoryEdit> = edits
            .into_iter()
            .rev()
            .filter(|e| seen.insert(e.local.clone()))
            .take(MAX_HISTORY_EDITS)
            .collect();
        kept.reverse();
        let mut insert = self.conn.prepare_cached(
            "INSERT OR IGNORE INTO message_revisions(message_id, account_id, net_author,
               net_local, revision, body, edited_at) VALUES (?1, ?2, ?3, ?4, 0, ?5, ?6)",
        )?;
        insert.execute(params![
            id.0,
            self.acc.id.0,
            author,
            m.local.as_ref(),
            None::<String>,
            None::<i64>
        ])?;
        let body = sql::to_json(&m.body)?;
        let newest = kept.len() - 1;
        for (i, edit) in kept.iter().enumerate() {
            insert.execute(params![
                id.0,
                self.acc.id.0,
                author,
                edit.local.as_ref(),
                (i == newest).then_some(body.as_str()),
                edit.at
            ])?;
        }
        drop(insert);
        self.reindex_revisions(id)?;
        Ok(kept.iter().map(|e| e.local.clone()).collect())
    }

    fn insert_attachments(&self, id: MessageId, body: &Body) -> Result<()> {
        let list: Vec<&kourier_model::Attachment> = match body {
            Body::Media { attachments, .. } => attachments.iter().collect(),
            Body::Sticker(a) => vec![a.as_ref()],
            _ => Vec::new(),
        };
        let mut stmt = self.conn.prepare_cached(
            "INSERT OR IGNORE INTO attachments(message_id, idx, kind, mime, size, file_name, width,
               height, duration_ms, blurhash, waveform, caption, state, remote, long_text)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
        )?;
        for (idx, a) in list.into_iter().enumerate() {
            stmt.execute(params![
                id.0,
                i64::try_from(idx).unwrap_or(i64::MAX),
                sql::enum_name(&a.kind)?,
                a.mime,
                sql::i64_of(a.size),
                a.file_name,
                a.dims.map(|d| d.0),
                a.dims.map(|d| d.1),
                a.duration_ms,
                a.blurhash,
                a.waveform,
                a.caption.as_ref().map(sql::to_json).transpose()?,
                sql::to_json(&a.state)?,
                a.remote.as_ref().map(sql::blob_to_sql),
                a.long_text,
            ])?;
        }
        Ok(())
    }

    /// Renumbers the revisions of a message by `edited_at` and copies the latest into the row
    /// (§3.5.3).
    pub(crate) fn reindex_revisions(&mut self, id: MessageId) -> Result<()> {
        let rows: Vec<(i64, Option<i64>, Option<String>)> = self
            .conn
            .prepare_cached(
                "SELECT rowid, edited_at, body FROM message_revisions WHERE message_id = ?1
                 ORDER BY edited_at IS NOT NULL, edited_at, net_local",
            )?
            .query_map([id.0], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<kourier_db::rusqlite::Result<_>>()?;
        if rows.is_empty() {
            return Ok(());
        }
        let mut upd = self
            .conn
            .prepare_cached("UPDATE message_revisions SET revision = ?2 WHERE rowid = ?1")?;
        for (i, (rowid, _, _)) in rows.iter().enumerate() {
            upd.execute((rowid, i64::try_from(i).unwrap_or(i64::MAX)))?;
        }
        let edits = i64::try_from(rows.len() - 1).unwrap_or(i64::MAX);
        let edited_at = rows.iter().filter_map(|r| r.1).max();
        let latest_body = rows.last().and_then(|r| r.2.clone());
        let deleted: Option<String> = self
            .conn
            .prepare_cached("SELECT deleted FROM messages WHERE id = ?1")?
            .query_row([id.0], |r| r.get(0))?;
        match (deleted, latest_body) {
            (None, Some(body_json)) => {
                let body: Body = sql::from_json(&body_json)?;
                self.conn
                    .prepare_cached(
                        "UPDATE messages SET body = ?2, body_kind = ?3, preview = ?4, revision = ?5,
                           edited_at = ?6 WHERE id = ?1",
                    )?
                    .execute(params![
                        id.0,
                        body_json,
                        kind_of(&body),
                        preview(&body),
                        edits,
                        edited_at
                    ])?;
            }
            _ => {
                self.conn
                    .prepare_cached(
                        "UPDATE messages SET revision = ?2, edited_at = ?3 WHERE id = ?1",
                    )?
                    .execute((id.0, edits, edited_at))?;
            }
        }
        Ok(())
    }

    /// Starts the `AfterRead` timer of a message that was just read (here or elsewhere), once.
    pub(crate) fn start_read_timer(&mut self, id: MessageId) -> Result<()> {
        let n = self
            .conn
            .prepare_cached(
                "UPDATE messages SET expiry_started_at = ?2,
                   expires_at = ?2 + expiry_timer_ms
                 WHERE id = ?1 AND expiry_kind = 'AfterRead' AND expiry_timer_ms IS NOT NULL
                   AND expiry_started_at IS NULL",
            )?
            .execute((id.0, self.now))?;
        if n > 0 {
            self.ch.expiring = true;
        }
        Ok(())
    }

    /// Starts the disappearing timer of an own message that just reached a recipient, once
    /// (whatever its kind: our own messages count from the send).
    pub(crate) fn start_send_timer(&mut self, id: MessageId) -> Result<()> {
        let n = self
            .conn
            .prepare_cached(
                "UPDATE messages SET expiry_started_at = ?2,
                   expires_at = ?2 + expiry_timer_ms
                 WHERE id = ?1 AND expiry_timer_ms IS NOT NULL AND expiry_started_at IS NULL",
            )?
            .execute((id.0, self.now))?;
        if n > 0 {
            self.ch.expiring = true;
        }
        Ok(())
    }

    /// Recomputes the aggregated outgoing state of an own message from its floor, per-recipient
    /// send states and receipts (§3.7).
    pub(crate) fn recompute_out(&mut self, id: MessageId) -> Result<()> {
        // (author, floor, state, conversation, is a notice)
        type Out = (String, Option<String>, Option<String>, i64, bool);
        let row: Option<Out> = self
            .conn
            .prepare_cached(
                "SELECT net_author, out_floor, out_state, conversation_id, body_kind = 'System'
                 FROM messages WHERE id = ?1",
            )?
            .query_row([id.0], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })
            .optional()?;
        let Some((author, floor, current, conv, notice)) = row else {
            return Ok(());
        };
        // Only messages we sent have an outgoing state: not those of others, and not the
        // notices of our own doing (a timer we changed), whatever send states name them.
        if !author.is_empty() || notice {
            return Ok(());
        }
        let floor: Option<OutgoingState> = sql::from_json_opt(floor)?;
        let mut recipients: BTreeMap<String, (Option<RecipientState>, Option<ReceiptKind>)> =
            BTreeMap::new();
        let sends: Vec<(String, String)> = self
            .conn
            .prepare_cached("SELECT user_id, state FROM send_state WHERE message_id = ?1")?
            .query_map([id.0], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<kourier_db::rusqlite::Result<_>>()?;
        for (u, s) in sends {
            recipients.entry(u).or_default().0 = Some(sql::from_json(&s)?);
        }
        let receipts: Vec<(String, String)> = self
            .conn
            .prepare_cached("SELECT user_id, kind FROM receipts WHERE message_id = ?1")?
            .query_map([id.0], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<kourier_db::rusqlite::Result<_>>()?;
        for (u, k) in receipts {
            let kind: ReceiptKind = sql::from_enum_name(&k)?;
            let e = recipients.entry(u).or_default();
            e.1 = Some(e.1.map_or(kind, |old| old.max(kind)));
        }
        let server_at = match &floor {
            Some(OutgoingState::Sent { server_at }) => *server_at,
            _ => None,
        };
        let aggregate = aggregate_out(&recipients, server_at);
        let state = merge_out(floor, aggregate);
        let json = state.as_ref().map(sql::to_json).transpose()?;
        if json != current {
            self.conn
                .prepare_cached("UPDATE messages SET out_state = ?2 WHERE id = ?1")?
                .execute((id.0, json))?;
            self.ch.message(ConversationId(conv), id);
        }
        Ok(())
    }
}

/// Whether an own message in `state` left this account's devices: everything but queued,
/// sending and failed (a partial failure reached somebody). The disappearing timer of an
/// own message counts from its send, so a message that was not sent has none running.
fn was_sent(state: &OutgoingState) -> bool {
    !matches!(
        state,
        OutgoingState::Queued | OutgoingState::Sending { .. } | OutgoingState::Failed { .. }
    )
}

/// Order of per-recipient send states (they only move forward): `Pending < Failed{retryable}
/// < Failed{permanent} < Sent{unsealed} < Sent{sealed}`.
pub(crate) fn recipient_rank(s: &RecipientState) -> u8 {
    match s {
        RecipientState::Pending => 0,
        RecipientState::Failed { retryable: true } => 1,
        RecipientState::Failed { retryable: false } => 2,
        RecipientState::Sent { sealed: false } => 3,
        RecipientState::Sent { sealed: true } => 4,
    }
}

/// The aggregate over recipients: all delivered → Delivered; a permanent failure next to
/// successes → PartiallyFailed (§3.7).
fn aggregate_out(
    recipients: &BTreeMap<String, (Option<RecipientState>, Option<ReceiptKind>)>,
    server_at: Option<i64>,
) -> Option<OutgoingState> {
    if recipients.is_empty() {
        return None;
    }
    let mut failed = Vec::new();
    let mut min_rank = u8::MAX;
    for (user, (send, receipt)) in recipients {
        let r = match (send, receipt) {
            (_, Some(ReceiptKind::Viewed)) => 5,
            (_, Some(ReceiptKind::Read)) => 4,
            (_, Some(ReceiptKind::Delivered)) => 3,
            (Some(RecipientState::Sent { .. }), None) => 2,
            (Some(RecipientState::Failed { retryable: true }), None) => 1,
            (Some(RecipientState::Pending), None) | (None, None) => 0,
            (Some(RecipientState::Failed { retryable: false }), None) => {
                if let Ok(u) = UserId::parse(user) {
                    failed.push(u);
                }
                continue;
            }
        };
        min_rank = min_rank.min(r);
    }
    if !failed.is_empty() {
        return Some(if min_rank == u8::MAX {
            OutgoingState::Failed {
                reason: kourier_model::FailReason::Other("failed for every recipient".into()),
                retryable: false,
            }
        } else {
            OutgoingState::PartiallyFailed { failed }
        });
    }
    Some(match min_rank {
        0 => OutgoingState::Queued,
        1 => OutgoingState::Sending { progress: None },
        2 => OutgoingState::Sent { server_at },
        3 => OutgoingState::Delivered,
        4 => OutgoingState::Read,
        _ => OutgoingState::Viewed,
    })
}

/// Merges the explicit floor with the aggregate: the later of the two in the forward order; a
/// failure beats `Queued`/`Sending`/`Sent`, a success at `Sent` or beyond beats a failure.
pub(crate) fn merge_out(
    floor: Option<OutgoingState>,
    aggregate: Option<OutgoingState>,
) -> Option<OutgoingState> {
    match (floor, aggregate) {
        (None, x) | (x, None) => x,
        (Some(f), Some(a)) => Some(match (f.rank(), a.rank()) {
            (Some(rf), Some(ra)) => {
                if ra > rf {
                    a
                } else {
                    f
                }
            }
            (Some(rf), None) => {
                if rf > 2 {
                    f
                } else {
                    a
                }
            }
            (None, Some(ra)) => {
                if ra >= 2 {
                    a
                } else {
                    f
                }
            }
            (None, None) => a,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uid(s: &str) -> String {
        s.to_owned()
    }

    #[test]
    fn aggregate_is_the_minimum_over_recipients() {
        let mut r = BTreeMap::new();
        r.insert(
            uid("aci:a"),
            (
                Some(RecipientState::Sent { sealed: true }),
                Some(ReceiptKind::Read),
            ),
        );
        r.insert(
            uid("aci:b"),
            (Some(RecipientState::Sent { sealed: false }), None),
        );
        assert_eq!(
            aggregate_out(&r, Some(5)),
            Some(OutgoingState::Sent { server_at: Some(5) })
        );
        if let Some(e) = r.get_mut("aci:b") {
            e.1 = Some(ReceiptKind::Delivered);
        }
        assert_eq!(aggregate_out(&r, None), Some(OutgoingState::Delivered));
        r.insert(
            uid("aci:c"),
            (Some(RecipientState::Failed { retryable: false }), None),
        );
        assert!(matches!(
            aggregate_out(&r, None),
            Some(OutgoingState::PartiallyFailed { failed }) if failed.len() == 1
        ));
    }

    #[test]
    fn merge_out_moves_forward_only() {
        let sent = Some(OutgoingState::Sent { server_at: None });
        assert_eq!(
            merge_out(Some(OutgoingState::Read), sent.clone()),
            Some(OutgoingState::Read)
        );
        assert_eq!(
            merge_out(sent.clone(), Some(OutgoingState::Delivered)),
            Some(OutgoingState::Delivered)
        );
        let failed = OutgoingState::PartiallyFailed { failed: vec![] };
        assert_eq!(merge_out(sent, Some(failed.clone())), Some(failed));
        assert!(preview(&Body::text("  hi  ")).is_some_and(|p| p == "hi"));
        assert_eq!(preview(&Body::text("")), None);
    }

    /// Sender-claimed timestamps at the extremes saturate instead of overflowing.
    #[test]
    fn history_sort_key_saturates_on_extreme_timestamps() {
        assert_eq!(history_sort_key(i64::MAX, "x"), i64::MAX);
        assert!(history_sort_key(i64::MAX / 1024, "y") >= i64::MAX - 1023);
        assert!(history_sort_key(i64::MIN, "z") <= i64::MIN + 1023);
        let k = history_sort_key(1_000, "a");
        assert!((1_024_000..1_025_024).contains(&k));
    }
}
