//! `IdentityMerged` and `merge_conversation` (§3.3).

use kourier_db::rusqlite::OptionalExtension;
use kourier_model::{ConvKey, ConversationId, MessageId, MsgIdScope, RecipientState, UserId};

use super::Apply;
use super::message::MsgRow;
use crate::error::Result;
use crate::sql;

/// Precedence of tombstones: delete-for-me hides the message for good.
fn deleted_rank(d: Option<&str>) -> u8 {
    match d {
        None => 0,
        Some("ForEveryone") => 1,
        Some(_) => 2,
    }
}

fn read_rank(s: Option<&str>) -> u8 {
    match s {
        Some("Viewed") => 3,
        Some("Read") => 2,
        Some("Unread") => 1,
        _ => 0,
    }
}

impl Apply<'_> {
    /// `from` is an alias of `into` (§3.3): rewrite authors, members, reactions, receipts, send
    /// states and parked modifiers, keep the alias row, then re-key the direct conversation of
    /// `from` or merge it into the one of `into`.
    pub(crate) fn identity_merged(&mut self, from: &UserId, into: &UserId) -> Result<()> {
        let target = self.resolve_user(into)?;
        if *from == target || self.is_own(from) || self.is_own(&target) {
            return Ok(());
        }
        let acc = self.acc.id.0;
        let (f, t) = (from.as_str(), target.as_str());
        self.conn
            .prepare_cached(
                "INSERT INTO identity_alias(account_id, alias, user_id) VALUES (?1, ?2, ?3)
                 ON CONFLICT(account_id, alias) DO UPDATE SET user_id = excluded.user_id",
            )?
            .execute((acc, f, t))?;
        // Aliases that pointed at `from` now point at the canonical id (no chains).
        self.conn
            .prepare_cached(
                "UPDATE identity_alias SET user_id = ?3 WHERE account_id = ?1 AND user_id = ?2",
            )?
            .execute((acc, f, t))?;

        self.merge_contact(from, &target)?;

        // Members: union, the canonical row wins.
        let convs: Vec<i64> = self
            .conn
            .prepare_cached(
                "SELECT m.conversation_id FROM members m JOIN conversations c ON c.id = m.conversation_id
                 WHERE c.account_id = ?1 AND m.user_id = ?2",
            )?
            .query_map((acc, f), |r| r.get(0))?
            .collect::<kourier_db::rusqlite::Result<_>>()?;
        for c in convs {
            self.conn
                .prepare_cached(
                    "UPDATE OR IGNORE members SET user_id = ?3 WHERE conversation_id = ?1 AND user_id = ?2",
                )?
                .execute((c, f, t))?;
            self.conn
                .prepare_cached("DELETE FROM members WHERE conversation_id = ?1 AND user_id = ?2")?
                .execute((c, f))?;
            self.ch.conversations.insert(ConversationId(c));
        }

        // Messages by `from`: re-author; under AccountAuthor a key conflict merges the rows.
        let touched: Vec<i64> = self
            .conn
            .prepare_cached(
                "SELECT DISTINCT conversation_id FROM messages WHERE account_id = ?1 AND net_author = ?2",
            )?
            .query_map((acc, f), |r| r.get(0))?
            .collect::<kourier_db::rusqlite::Result<_>>()?;
        if self.scope == MsgIdScope::AccountAuthor {
            let rows: Vec<(i64, String)> = self
                .conn
                .prepare_cached(
                    "SELECT id, net_local FROM messages WHERE account_id = ?1 AND net_author = ?2 ORDER BY id",
                )?
                .query_map((acc, f), |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<kourier_db::rusqlite::Result<_>>()?;
            for (id, local) in rows {
                let other: Option<i64> = self
                    .conn
                    .prepare_cached(
                        "SELECT id FROM messages WHERE account_id = ?1 AND net_author = ?2
                           AND net_local = ?3 AND net_scope = 0",
                    )?
                    .query_row((acc, t, &local), |r| r.get(0))
                    .optional()?;
                if let Some(other) = other {
                    let (Some(a), Some(b)) = (
                        self.msg_row(MessageId(id))?,
                        self.msg_row(MessageId(other))?,
                    ) else {
                        continue;
                    };
                    // Keep the older row (§3.3 step 1).
                    let (keep, drop) = if (a.sort_key, a.id) <= (b.sort_key, b.id) {
                        (a, b)
                    } else {
                        (b, a)
                    };
                    self.merge_rows(&keep, &drop)?;
                }
            }
        }
        self.conn
            .prepare_cached(
                "UPDATE messages SET net_author = ?3 WHERE account_id = ?1 AND net_author = ?2",
            )?
            .execute((acc, f, t))?;
        self.conn
            .prepare_cached(
                "UPDATE OR IGNORE message_revisions SET net_author = ?3 WHERE account_id = ?1 AND net_author = ?2",
            )?
            .execute((acc, f, t))?;
        self.conn
            .prepare_cached(
                "UPDATE messages SET deleted_by = ?3 WHERE account_id = ?1 AND deleted_by = ?2",
            )?
            .execute((acc, f, t))?;
        for c in touched {
            self.ch.timelines.insert(ConversationId(c));
            self.ch.recount_conv(ConversationId(c));
        }

        self.rewrite_user_rows(f, t)?;

        // Parked modifiers follow the canonical id.
        self.conn
            .prepare_cached(
                "UPDATE OR IGNORE pending_modifiers SET target_author = ?3
                 WHERE account_id = ?1 AND target_author = ?2",
            )?
            .execute((acc, f, t))?;
        let (from_key, into_key) = (
            ConvKey::Direct(from.clone()).encode(),
            ConvKey::Direct(target.clone()).encode(),
        );
        self.conn
            .prepare_cached(
                "UPDATE OR IGNORE pending_modifiers SET conv_key = ?3 WHERE account_id = ?1 AND conv_key = ?2",
            )?
            .execute((acc, &from_key, &into_key))?;
        // So do the keys of expired messages: a late copy under either identity stays dropped.
        self.conn
            .prepare_cached(
                "UPDATE expired_messages SET net_author = ?3 WHERE account_id = ?1 AND net_author = ?2",
            )?
            .execute((acc, f, t))?;
        self.conn
            .prepare_cached(
                "UPDATE expired_messages SET conv_key = ?3 WHERE account_id = ?1 AND conv_key = ?2",
            )?
            .execute((acc, &from_key, &into_key))?;

        // The direct conversation: re-key, or merge when `into` has one already.
        let from_conv =
            sql::conversation_id(self.conn, self.acc.id, &ConvKey::Direct(from.clone()))?;
        let into_conv = sql::conversation_id(self.conn, self.acc.id, &ConvKey::Direct(target))?;
        match (from_conv, into_conv) {
            (Some(fc), Some(ic)) => self.merge_conversation(fc, ic)?,
            (Some(fc), None) => {
                self.conn
                    .prepare_cached("UPDATE conversations SET conv_key = ?2 WHERE id = ?1")?
                    .execute((fc.0, &into_key))?;
                self.ch.conversations.insert(fc);
                self.ch.timelines.insert(fc);
            }
            _ => {}
        }
        // A modifier that was sent in the chat under one identity may name a message that was
        // in the chat under the other until now: both are one conversation from here on.
        self.retry_pending(&into_key)?;
        // And a receipt that waited because its sender was no recipient of its message may
        // come from one now.
        self.retry_receipts()?;
        self.ch.contacts = true;
        Ok(())
    }

    /// Merges the contact of `from` into the one of `into`: fields of `into` win, empty ones are
    /// filled from `from`.
    fn merge_contact(&mut self, from: &UserId, into: &UserId) -> Result<()> {
        let Some((f, f_avatar)) = self.load_contact(from)? else {
            return Ok(());
        };
        let acc = self.acc.id.0;
        match self.load_contact(into)? {
            None => {
                self.conn
                    .prepare_cached(
                        "UPDATE contacts SET user_id = ?3 WHERE account_id = ?1 AND user_id = ?2",
                    )?
                    .execute((acc, from.as_str(), into.as_str()))?;
            }
            Some((mut t, t_avatar)) => {
                t.profile_name = t.profile_name.or(f.profile_name);
                t.system_name = t.system_name.or(f.system_name);
                t.nickname = t.nickname.or(f.nickname);
                t.identity_fingerprint = t.identity_fingerprint.or(f.identity_fingerprint);
                t.unregistered_at = t.unregistered_at.or(f.unregistered_at);
                t.meta = t.meta.or(f.meta);
                if t.identifiers.is_empty() {
                    t.identifiers = f.identifiers;
                }
                t.trust = t.trust.max(f.trust);
                t.blocked |= f.blocked;
                t.hidden &= f.hidden;
                let avatar = t_avatar.or(f_avatar);
                self.conn
                    .prepare_cached("DELETE FROM contacts WHERE account_id = ?1 AND user_id = ?2")?
                    .execute((acc, from.as_str()))?;
                self.store_contact(&t, avatar)?;
                if let (Some(fa), Some(_)) = (f_avatar, t_avatar) {
                    self.conn
                        .prepare_cached("DELETE FROM files WHERE id = ?1")?
                        .execute([fa])?;
                }
            }
        }
        Ok(())
    }

    /// Reactions, receipts and send states of `from` become `into`'s, merged with the same
    /// rules as live events.
    fn rewrite_user_rows(&mut self, from: &str, into: &str) -> Result<()> {
        let acc = self.acc.id.0;
        let reactions: Vec<(i64, String, i64, bool)> = self
            .conn
            .prepare_cached(
                "SELECT r.message_id, r.emoji, r.at, r.removed FROM reactions r
                   JOIN messages m ON m.id = r.message_id
                 WHERE m.account_id = ?1 AND r.user_id = ?2",
            )?
            .query_map((acc, from), |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })?
            .collect::<kourier_db::rusqlite::Result<_>>()?;
        for (mid, emoji, at, removed) in reactions {
            self.merge_reaction(MessageId(mid), into, &emoji, at, removed)?;
        }
        self.conn
            .prepare_cached(
                "DELETE FROM reactions WHERE user_id = ?2
                   AND message_id IN (SELECT id FROM messages WHERE account_id = ?1)",
            )?
            .execute((acc, from))?;

        let receipts: Vec<(i64, String, i64)> = self
            .conn
            .prepare_cached(
                "SELECT r.message_id, r.kind, r.at FROM receipts r JOIN messages m ON m.id = r.message_id
                 WHERE m.account_id = ?1 AND r.user_id = ?2",
            )?
            .query_map((acc, from), |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<kourier_db::rusqlite::Result<_>>()?;
        let mut own_messages: Vec<i64> = Vec::new();
        for (mid, kind, at) in receipts {
            self.merge_receipt(MessageId(mid), into, &kind, at)?;
            own_messages.push(mid);
        }
        self.conn
            .prepare_cached(
                "DELETE FROM receipts WHERE user_id = ?2
                   AND message_id IN (SELECT id FROM messages WHERE account_id = ?1)",
            )?
            .execute((acc, from))?;

        let sends: Vec<(i64, String, Option<String>)> = self
            .conn
            .prepare_cached(
                "SELECT s.message_id, s.state, s.error FROM send_state s JOIN messages m ON m.id = s.message_id
                 WHERE m.account_id = ?1 AND s.user_id = ?2",
            )?
            .query_map((acc, from), |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<kourier_db::rusqlite::Result<_>>()?;
        for (mid, state, error) in sends {
            let state: RecipientState = sql::from_json(&state)?;
            self.merge_send_state(MessageId(mid), into, &state, error.as_deref())?;
            own_messages.push(mid);
        }
        self.conn
            .prepare_cached(
                "DELETE FROM send_state WHERE user_id = ?2
                   AND message_id IN (SELECT id FROM messages WHERE account_id = ?1)",
            )?
            .execute((acc, from))?;
        own_messages.sort_unstable();
        own_messages.dedup();
        for mid in own_messages {
            self.recompute_out(MessageId(mid))?;
        }
        Ok(())
    }

    /// Moves everything of `from` into `into` and deletes `from` (§3.3 steps 1-5).
    pub(crate) fn merge_conversation(
        &mut self,
        from: ConversationId,
        into: ConversationId,
    ) -> Result<()> {
        if from == into {
            return Ok(());
        }
        let acc = self.acc.id.0;
        // 1. Messages; per-conversation keys may collide: every key of a network whose ids
        // count per conversation, and on any network the keys of call lines
        // ([`super::call`]). Of two lines of one call id the older row stays (their states
        // are not joined: merges that involve a call line are not designed yet).
        {
            let rows: Vec<(i64, String)> = self
                .conn
                .prepare_cached(
                    "SELECT id, net_local FROM messages WHERE conversation_id = ?1
                       AND net_scope = 1 ORDER BY id",
                )?
                .query_map([from.0], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<kourier_db::rusqlite::Result<_>>()?;
            for (id, local) in rows {
                let other: Option<i64> = self
                    .conn
                    .prepare_cached(
                        "SELECT id FROM messages WHERE conversation_id = ?1 AND net_local = ?2 AND net_scope = 1",
                    )?
                    .query_row((into.0, &local), |r| r.get(0))
                    .optional()?;
                let Some(other) = other else { continue };
                let (Some(a), Some(b)) = (
                    self.msg_row(MessageId(id))?,
                    self.msg_row(MessageId(other))?,
                ) else {
                    continue;
                };
                let (keep, drop) = if (a.sort_key, a.id) <= (b.sort_key, b.id) {
                    (a, b)
                } else {
                    (b, a)
                };
                self.merge_rows(&keep, &drop)?;
            }
        }
        self.conn
            .prepare_cached("UPDATE messages SET conversation_id = ?2 WHERE conversation_id = ?1")?
            .execute((from.0, into.0))?;
        self.conn
            .prepare_cached("UPDATE outbox SET conversation_id = ?2 WHERE conversation_id = ?1")?
            .execute((from.0, into.0))?;

        // 2. Parked modifiers and members.
        let keys: (String, String) = self
            .conn
            .prepare_cached(
                "SELECT f.conv_key, i.conv_key FROM conversations f, conversations i
                 WHERE f.id = ?1 AND i.id = ?2",
            )?
            .query_row((from.0, into.0), |r| Ok((r.get(0)?, r.get(1)?)))?;
        self.conn
            .prepare_cached(
                "UPDATE OR IGNORE pending_modifiers SET conv_key = ?3 WHERE account_id = ?1 AND conv_key = ?2",
            )?
            .execute((acc, &keys.0, &keys.1))?;
        self.conn
            .prepare_cached(
                "INSERT OR IGNORE INTO members(conversation_id, user_id, role)
                 SELECT ?2, user_id, role FROM members WHERE conversation_id = ?1",
            )?
            .execute((from.0, into.0))?;

        // 3. Fields: roaming fields of `into` win (empty ones are filled), local state merges.
        let avatars: (Option<i64>, Option<i64>) = self
            .conn
            .prepare_cached(
                "SELECT f.avatar_file, i.avatar_file FROM conversations f, conversations i
                 WHERE f.id = ?1 AND i.id = ?2",
            )?
            .query_row((from.0, into.0), |r| Ok((r.get(0)?, r.get(1)?)))?;
        self.conn
            .prepare_cached(
                "UPDATE conversations AS i SET
                   title = coalesce(i.title, f.title),
                   avatar_file = coalesce(i.avatar_file, f.avatar_file),
                   muted_until = coalesce(i.muted_until, f.muted_until),
                   pinned_order = coalesce(i.pinned_order, f.pinned_order),
                   timer_secs = CASE
                     WHEN coalesce(f.timer_version, -1) > coalesce(i.timer_version, -1)
                       THEN f.timer_secs
                     WHEN i.timer_version IS NOT NULL THEN i.timer_secs
                     ELSE coalesce(i.timer_secs, f.timer_secs) END,
                   timer_version = CASE
                     WHEN coalesce(f.timer_version, -1) > coalesce(i.timer_version, -1)
                       THEN f.timer_version
                     ELSE i.timer_version END,
                   features = coalesce(i.features, f.features),
                   features_hash = coalesce(i.features_hash, f.features_hash),
                   meta = coalesce(i.meta, f.meta),
                   draft = coalesce(i.draft, f.draft),
                   local_muted_until = CASE
                     WHEN i.local_muted_until IS NULL THEN f.local_muted_until
                     WHEN f.local_muted_until IS NULL THEN i.local_muted_until
                     ELSE max(i.local_muted_until, f.local_muted_until) END,
                   read_up_to = CASE
                     WHEN i.read_up_to IS NULL THEN f.read_up_to
                     WHEN f.read_up_to IS NULL THEN i.read_up_to
                     ELSE max(i.read_up_to, f.read_up_to) END,
                   read_sent_at = CASE
                     WHEN i.read_sent_at IS NULL THEN f.read_sent_at
                     WHEN f.read_sent_at IS NULL THEN i.read_sent_at
                     ELSE max(i.read_sent_at, f.read_sent_at) END,
                   unread_hint = i.unread_hint + f.unread_hint,
                   -- Deleted on another device (`ConversationCleared`) only when both sides
                   -- were: the messages of a side that is listed must not vanish from the
                   -- list by joining a chat that was deleted under the other identity.
                   hidden = min(i.hidden, f.hidden)
                 FROM (SELECT * FROM conversations WHERE id = ?1) AS f
                 WHERE i.id = ?2",
            )?
            .execute((from.0, into.0))?;
        if let (Some(fa), Some(_)) = avatars {
            // `into` kept its own avatar; the other one is unreferenced now.
            self.conn
                .prepare_cached("UPDATE conversations SET avatar_file = NULL WHERE id = ?1")?
                .execute([from.0])?;
            self.conn
                .prepare_cached("DELETE FROM files WHERE id = ?1")?
                .execute([fa])?;
        }
        // The merged read watermark covers messages of both sides.
        let watermark: Option<i64> = self
            .conn
            .prepare_cached("SELECT read_sent_at FROM conversations WHERE id = ?1")?
            .query_row([into.0], |r| r.get(0))?;
        if let Some(w) = watermark {
            let read: Vec<i64> = self
                .conn
                .prepare_cached(super::READ_UP_TO)?
                .query_map((into.0, w), |r| r.get(0))?
                .collect::<kourier_db::rusqlite::Result<_>>()?;
            // Read is read: their `AfterRead` timers start like after any other read.
            for id in read {
                self.start_read_timer(MessageId(id))?;
            }
        }
        self.conn
            .prepare_cached("UPDATE conversations SET parent_id = ?2 WHERE parent_id = ?1")?
            .execute((from.0, into.0))?;

        // 4. Delete `from` (members cascade; the alias row stays).
        self.conn
            .prepare_cached("DELETE FROM conversations WHERE id = ?1")?
            .execute([from.0])?;

        // 5. Invalidations.
        self.ch.conversations.remove(&from);
        self.ch.messages.remove(&from);
        self.ch.timelines.remove(&from);
        self.ch.recount.remove(&from);
        self.ch.merged.push((from, into));
        self.ch.conversations.insert(into);
        self.ch.timelines.insert(into);
        self.ch.recount_conv(into);
        Ok(())
    }

    /// Merges message `drop` into `keep` (same key) monotonically and deletes `drop`.
    fn merge_rows(&mut self, keep: &MsgRow, drop: &MsgRow) -> Result<()> {
        type State = (
            Option<String>,
            Option<String>,
            Option<String>,
            String,
            String,
        );
        let load = |id: MessageId| -> Result<State> {
            Ok(self
                .conn
                .prepare_cached(
                    "SELECT out_floor, read_state, deleted, body_kind, body FROM messages WHERE id = ?1",
                )?
                .query_row([id.0], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?)
        };
        let k = load(keep.id)?;
        let d = load(drop.id)?;
        // The disappearing timer of whichever copy carried one: a copy that arrived without it
        // must not make the message permanent. With two, the kept row's timer stands and the
        // earlier start and end count. Before the read state below, which starts an `AfterRead`
        // timer that did not run yet.
        self.conn
            .prepare_cached(
                "UPDATE messages SET
                   expiry_kind = coalesce(messages.expiry_kind, d.expiry_kind),
                   expiry_timer_ms = coalesce(messages.expiry_timer_ms, d.expiry_timer_ms),
                   expiry_started_at = CASE
                     WHEN messages.expiry_started_at IS NULL THEN d.expiry_started_at
                     WHEN d.expiry_started_at IS NULL THEN messages.expiry_started_at
                     ELSE min(messages.expiry_started_at, d.expiry_started_at) END,
                   expires_at = CASE
                     WHEN messages.expires_at IS NULL THEN d.expires_at
                     WHEN d.expires_at IS NULL THEN messages.expires_at
                     ELSE min(messages.expires_at, d.expires_at) END
                 FROM (SELECT expiry_kind, expiry_timer_ms, expiry_started_at, expires_at
                       FROM messages WHERE id = ?2) AS d
                 WHERE messages.id = ?1",
            )?
            .execute((keep.id.0, drop.id.0))?;
        let expires: Option<i64> = self
            .conn
            .prepare_cached("SELECT expires_at FROM messages WHERE id = ?1")?
            .query_row([keep.id.0], |r| r.get(0))?;
        if expires.is_some() {
            self.ch.expiring = true;
        }
        let floor = super::message::merge_out(
            sql::from_json_opt(k.0.clone())?,
            sql::from_json_opt(d.0.clone())?,
        );
        let read_state = if read_rank(d.1.as_deref()) > read_rank(k.1.as_deref()) {
            d.1.clone()
        } else {
            k.1.clone()
        };
        self.conn
            .prepare_cached("UPDATE messages SET out_floor = ?2, read_state = ?3 WHERE id = ?1")?
            .execute((
                keep.id.0,
                floor.as_ref().map(sql::to_json).transpose()?,
                &read_state,
            ))?;
        if read_rank(read_state.as_deref()) > read_rank(Some("Unread")) {
            // The kept row is read now, if it was not before: its `AfterRead` timer starts.
            self.start_read_timer(keep.id)?;
        }
        if k.3 == "Placeholder" && d.3 != "Placeholder" && k.2.is_none() && d.2.is_none() {
            self.conn
                .prepare_cached(
                    "UPDATE messages SET body = m.body, body_kind = m.body_kind, preview = m.preview,
                       reply_to = m.reply_to
                     FROM (SELECT body, body_kind, preview, reply_to FROM messages WHERE id = ?2) AS m
                     WHERE messages.id = ?1",
                )?
                .execute((keep.id.0, drop.id.0))?;
            // The real body's attachments come with it (the placeholder has none).
            self.delete_attachments(keep.id)?;
            self.conn
                .prepare_cached("UPDATE attachments SET message_id = ?1 WHERE message_id = ?2")?
                .execute((keep.id.0, drop.id.0))?;
        }
        // Revisions, reactions, receipts, send states move over.
        self.conn
            .prepare_cached(
                "INSERT OR IGNORE INTO message_revisions(message_id, account_id, net_author, net_local,
                   revision, body, edited_at)
                 SELECT ?1, account_id, net_author, net_local, revision, body, edited_at
                 FROM message_revisions WHERE message_id = ?2",
            )?
            .execute((keep.id.0, drop.id.0))?;
        let reactions: Vec<(String, String, i64, bool)> = self
            .conn
            .prepare_cached(
                "SELECT user_id, emoji, at, removed FROM reactions WHERE message_id = ?1",
            )?
            .query_map([drop.id.0], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })?
            .collect::<kourier_db::rusqlite::Result<_>>()?;
        for (u, e, at, removed) in reactions {
            self.merge_reaction(keep.id, &u, &e, at, removed)?;
        }
        let receipts: Vec<(String, String, i64)> = self
            .conn
            .prepare_cached("SELECT user_id, kind, at FROM receipts WHERE message_id = ?1")?
            .query_map([drop.id.0], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<kourier_db::rusqlite::Result<_>>()?;
        for (u, kind, at) in receipts {
            self.merge_receipt(keep.id, &u, &kind, at)?;
        }
        let sends: Vec<(String, String, Option<String>)> = self
            .conn
            .prepare_cached("SELECT user_id, state, error FROM send_state WHERE message_id = ?1")?
            .query_map([drop.id.0], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<kourier_db::rusqlite::Result<_>>()?;
        for (u, state, error) in sends {
            let state: RecipientState = sql::from_json(&state)?;
            self.merge_send_state(keep.id, &u, &state, error.as_deref())?;
        }
        self.conn
            .prepare_cached(
                "UPDATE messages SET linked_message_id = ?2 WHERE linked_message_id = ?1",
            )?
            .execute((drop.id.0, keep.id.0))?;
        self.conn
            .prepare_cached("UPDATE outbox SET message_id = ?2 WHERE message_id = ?1")?
            .execute((drop.id.0, keep.id.0))?;
        // A tombstone on either side wins.
        if deleted_rank(d.2.as_deref()) > deleted_rank(k.2.as_deref()) {
            self.conn
                .prepare_cached(
                    "UPDATE messages SET deleted = m.deleted, deleted_by = m.deleted_by, body = m.body,
                       body_kind = m.body_kind, preview = NULL, reply_to = NULL, raw = NULL
                     FROM (SELECT deleted, deleted_by, body, body_kind FROM messages WHERE id = ?2) AS m
                     WHERE messages.id = ?1",
                )?
                .execute((keep.id.0, drop.id.0))?;
            self.conn
                .prepare_cached("UPDATE message_revisions SET body = NULL WHERE message_id = ?1")?
                .execute([keep.id.0])?;
            self.conn
                .prepare_cached("DELETE FROM reactions WHERE message_id = ?1")?
                .execute([keep.id.0])?;
        }
        self.delete_attachments(drop.id)?;
        self.conn
            .prepare_cached("DELETE FROM messages WHERE id = ?1")?
            .execute([drop.id.0])?;
        self.reindex_revisions(keep.id)?;
        self.recompute_out(keep.id)?;
        self.ch.message(keep.conv, keep.id);
        self.ch.timelines.insert(drop.conv);
        self.ch.recount_conv(drop.conv);
        Ok(())
    }
}
