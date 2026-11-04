//! `IdentityMerged` and `merge_conversation` (§3.3).

use std::collections::HashMap;

use kourier_db::rusqlite::OptionalExtension;
use kourier_model::{
    ConvKey, ConversationId, MessageId, MsgIdScope, RecipientState, TrustLevel, UserId,
};

use super::Apply;
use super::message::MsgRow;
use crate::error::Result;
use crate::sql;

// The statements of [`Apply::identity_merged`] that look for the rows of the alias. Each
// starts from the alias's own rows through an index (core.db schema 14), never from all of
// the account's messages: the first Storage Service sync after a link merges every contact
// with a phone number, and a walk of the messages per merge took minutes on a big account
// (`tests::a_merge_looks_only_at_the_rows_of_the_alias` holds the plans). The joins are
// `CROSS JOIN`s because core.db has no planner statistics and the planner would start from
// the account's messages; for the same reason an `a = ? OR b = ?` is two `EXISTS`, one per
// index (given the OR the planner took the index of `account_id` alone and walked them all).

/// Whether `?2` (with direct conversation key `?3`) owns anything of account `?1` that a
/// merge would move: without, a merge only records the alias.
pub(crate) const ALIAS_OWNS: &str = "SELECT
    EXISTS(SELECT 1 FROM contacts WHERE account_id = ?1 AND user_id = ?2)
 OR EXISTS(SELECT 1 FROM members mb CROSS JOIN conversations c ON c.id = mb.conversation_id
           WHERE mb.user_id = ?2 AND c.account_id = ?1)
 OR EXISTS(SELECT 1 FROM messages WHERE account_id = ?1 AND net_author = ?2)
 OR EXISTS(SELECT 1 FROM messages WHERE account_id = ?1 AND deleted_by = ?2)
 OR EXISTS(SELECT 1 FROM message_revisions WHERE account_id = ?1 AND net_author = ?2)
 OR EXISTS(SELECT 1 FROM reactions r CROSS JOIN messages m ON m.id = r.message_id
           WHERE r.user_id = ?2 AND m.account_id = ?1)
 OR EXISTS(SELECT 1 FROM receipts r CROSS JOIN messages m ON m.id = r.message_id
           WHERE r.user_id = ?2 AND m.account_id = ?1)
 OR EXISTS(SELECT 1 FROM send_state s CROSS JOIN messages m ON m.id = s.message_id
           WHERE s.user_id = ?2 AND m.account_id = ?1)
 OR EXISTS(SELECT 1 FROM pending_modifiers WHERE account_id = ?1 AND target_author = ?2)
 OR EXISTS(SELECT 1 FROM pending_modifiers WHERE account_id = ?1 AND conv_key = ?3)
 OR EXISTS(SELECT 1 FROM expired_messages WHERE account_id = ?1 AND net_author = ?2)
 OR EXISTS(SELECT 1 FROM expired_messages WHERE account_id = ?1 AND conv_key = ?3)
 OR EXISTS(SELECT 1 FROM conversation_cuts WHERE account_id = ?1 AND conv_key = ?3)
 OR EXISTS(SELECT 1 FROM conversations WHERE account_id = ?1 AND conv_key = ?3)";

/// Aliases that pointed at `?2` point at the canonical id `?3` (no chains).
const ALIAS_REPOINT: &str =
    "UPDATE identity_alias SET user_id = ?3 WHERE account_id = ?1 AND user_id = ?2";
const MEMBER_CONVS: &str = "SELECT mb.conversation_id FROM members mb
   CROSS JOIN conversations c ON c.id = mb.conversation_id
 WHERE mb.user_id = ?2 AND c.account_id = ?1";
const AUTHORED_CONVS: &str =
    "SELECT DISTINCT conversation_id FROM messages WHERE account_id = ?1 AND net_author = ?2";
const AUTHORED_ROWS: &str =
    "SELECT id, net_local FROM messages WHERE account_id = ?1 AND net_author = ?2 ORDER BY id";
// Per-sender ids (`MsgIdScope::ConversationAuthor`, core.db schema 16): the key of a message is
// `(conversation, author, local)`, so a merge of two identities can meet the same message
// twice, in one conversation (a group) or in the two direct chats it joins. The rows of the
// alias come through `messages_by_author`, the other copy through the partial key
// `messages_key_conv_author` (it says `net_scope = 2`, or the planner cannot use it).
const AUTHORED_ROWS_PER_SENDER: &str = "SELECT id, conversation_id, net_local FROM messages
 WHERE account_id = ?1 AND net_author = ?2 AND net_scope = 2 ORDER BY id";
const SENDER_KEY_IN_CONV: &str = "SELECT id FROM messages
 WHERE conversation_id = ?1 AND net_author = ?2 AND net_local = ?3 AND net_scope = 2";
const CONV_ROWS_PER_SENDER: &str = "SELECT id, net_author, net_local FROM messages
 WHERE conversation_id = ?1 AND net_scope = 2 ORDER BY id";
const REAUTHOR: &str =
    "UPDATE messages SET net_author = ?3 WHERE account_id = ?1 AND net_author = ?2";
const REAUTHOR_REVISIONS: &str = "UPDATE OR IGNORE message_revisions SET net_author = ?3
 WHERE account_id = ?1 AND net_author = ?2";
const REDELETER: &str =
    "UPDATE messages SET deleted_by = ?3 WHERE account_id = ?1 AND deleted_by = ?2";
const PENDING_RETARGET: &str = "UPDATE OR IGNORE pending_modifiers SET target_author = ?3
 WHERE account_id = ?1 AND target_author = ?2";
const PENDING_REKEY: &str =
    "UPDATE OR IGNORE pending_modifiers SET conv_key = ?3 WHERE account_id = ?1 AND conv_key = ?2";
const EXPIRED_REAUTHOR: &str =
    "UPDATE expired_messages SET net_author = ?3 WHERE account_id = ?1 AND net_author = ?2";
const EXPIRED_REKEY: &str =
    "UPDATE expired_messages SET conv_key = ?3 WHERE account_id = ?1 AND conv_key = ?2";
// The cut of a conversation key (core.db schema 16, `ingest/clear.rs`) follows the alias like the
// keys of expired messages do: an UPSERT that keeps the larger of the two cuts, then the old
// key's row goes. A plain `UPDATE ... SET conv_key` would meet the primary key when both keys
// have a cut, and a constraint error fails the envelope for good. Both are primary-key lookups.
const CUTS_REKEY: &str =
    "INSERT INTO conversation_cuts(account_id, conv_key, before_ms, claimed_ms)
 SELECT account_id, ?3, before_ms, claimed_ms FROM conversation_cuts
 WHERE account_id = ?1 AND conv_key = ?2
 ON CONFLICT(account_id, conv_key) DO UPDATE SET
   before_ms = max(before_ms, excluded.before_ms),
   claimed_ms = max(claimed_ms, excluded.claimed_ms)";
// The key a row would get when a merge re-authors it or moves it to the other chat is looked at in
// the memory of removed messages (`expiry::remembered`, whose three statements are in `LOOKUPS`):
// a row whose message was removed under that key is removed, not brought back (the per-sender
// passes, the scope-1 pass and the account-wide pass of `AccountAuthor`). One look-up per
// row the alias owns and no live twin, on the indexes of migration 14 (they end in `net_local`).
const CONV_KEY_OF: &str = "SELECT conv_key FROM conversations WHERE id = ?1";
const CUTS_DROP: &str = "DELETE FROM conversation_cuts WHERE account_id = ?1 AND conv_key = ?2";
const REACTIONS_OF: &str = "SELECT r.message_id, r.emoji, r.at, r.removed FROM reactions r
   CROSS JOIN messages m ON m.id = r.message_id
 WHERE r.user_id = ?2 AND m.account_id = ?1";
const DELETE_REACTIONS_OF: &str = "DELETE FROM reactions WHERE user_id = ?2
   AND EXISTS(SELECT 1 FROM messages m WHERE m.id = reactions.message_id AND m.account_id = ?1)";
const RECEIPTS_OF: &str = "SELECT r.message_id, r.kind, r.at FROM receipts r
   CROSS JOIN messages m ON m.id = r.message_id
 WHERE r.user_id = ?2 AND m.account_id = ?1";
const DELETE_RECEIPTS_OF: &str = "DELETE FROM receipts WHERE user_id = ?2
   AND EXISTS(SELECT 1 FROM messages m WHERE m.id = receipts.message_id AND m.account_id = ?1)";
const SENDS_OF: &str = "SELECT s.message_id, s.state, s.error FROM send_state s
   CROSS JOIN messages m ON m.id = s.message_id
 WHERE s.user_id = ?2 AND m.account_id = ?1";
const DELETE_SENDS_OF: &str = "DELETE FROM send_state WHERE user_id = ?2
   AND EXISTS(SELECT 1 FROM messages m WHERE m.id = send_state.message_id AND m.account_id = ?1)";
// The other names of a dropped row are names of the kept one (KT-2): a key lookup of `message_id`.
// (`OR IGNORE`: a name both have stays the kept row's, and the dropped row's goes with its row.)
const ALIASES_FOLLOW: &str = "UPDATE OR IGNORE message_aliases SET message_id = ?1
 WHERE message_id = ?2";

/// Every statement above, for the plan test.
#[cfg(test)]
pub(crate) const LOOKUPS: &[&str] = &[
    ALIAS_OWNS,
    ALIAS_REPOINT,
    MEMBER_CONVS,
    AUTHORED_CONVS,
    AUTHORED_ROWS,
    AUTHORED_ROWS_PER_SENDER,
    SENDER_KEY_IN_CONV,
    CONV_ROWS_PER_SENDER,
    REAUTHOR,
    REAUTHOR_REVISIONS,
    REDELETER,
    PENDING_RETARGET,
    PENDING_REKEY,
    EXPIRED_REAUTHOR,
    EXPIRED_REKEY,
    CUTS_REKEY,
    CUTS_DROP,
    CONV_KEY_OF,
    crate::expiry::REMEMBERED_BY_AUTHOR,
    crate::expiry::REMEMBERED_IN_CONV,
    crate::expiry::REMEMBERED_IN_CONV_BY_AUTHOR,
    REACTIONS_OF,
    DELETE_REACTIONS_OF,
    RECEIPTS_OF,
    DELETE_RECEIPTS_OF,
    SENDS_OF,
    DELETE_SENDS_OF,
    ALIASES_FOLLOW,
];

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
        self.conn
            .prepare_cached(ALIAS_REPOINT)?
            .execute((acc, f, t))?;

        let (from_key, into_key) = (
            ConvKey::Direct(from.clone()).encode(),
            ConvKey::Direct(target.clone()).encode(),
        );
        let owns: bool = self
            .conn
            .prepare_cached(ALIAS_OWNS)?
            .query_row((acc, f, &from_key), |r| r.get(0))?;
        if !owns {
            // Nothing to move (on a first Storage Service sync: every contact with a number
            // whose number-user never wrote here). A receipt the alias sent may have waited
            // for its sender to be a recipient: it is one now (looked at in `finish`).
            self.ch.receipts_to_retry = true;
            self.ch.contacts = true;
            return Ok(());
        }

        self.merge_contact(from, &target)?;

        // Members: union, the canonical row wins.
        let convs: Vec<i64> = self
            .conn
            .prepare_cached(MEMBER_CONVS)?
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

        // Messages by `from`: re-author; under AccountAuthor and ConversationAuthor a key
        // conflict merges the rows (REAUTHOR below would break the unique key otherwise, and
        // a constraint error fails the whole envelope).
        let touched: Vec<i64> = self
            .conn
            .prepare_cached(AUTHORED_CONVS)?
            .query_map((acc, f), |r| r.get(0))?
            .collect::<kourier_db::rusqlite::Result<_>>()?;
        if self.scope == MsgIdScope::ConversationAuthor {
            let rows: Vec<(i64, i64, String)> = self
                .conn
                .prepare_cached(AUTHORED_ROWS_PER_SENDER)?
                .query_map((acc, f), |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<kourier_db::rusqlite::Result<_>>()?;
            // The key each chat has after the merge: a group keeps its key, the direct chat of
            // `from` becomes the one of `into`.
            let mut dest: HashMap<i64, String> = HashMap::new();
            for (id, conv, local) in rows {
                let other: Option<i64> = self
                    .conn
                    .prepare_cached(SENDER_KEY_IN_CONV)?
                    .query_row((conv, t, &local), |r| r.get(0))
                    .optional()?;
                if let Some(other) = other {
                    self.merge_older(id, other)?;
                    continue;
                }
                // No live twin. Re-authoring the row would make it exactly the key of a message
                // that was REMOVED under the canonical author (a clear, a delete for me, the
                // sweeper): it must not come back, as a later copy would not (it is remembered).
                if let std::collections::hash_map::Entry::Vacant(slot) = dest.entry(conv) {
                    let key = self.conv_key_of(conv)?;
                    slot.insert(if key == from_key {
                        into_key.clone()
                    } else {
                        key
                    });
                }
                if crate::expiry::remembered(
                    self.conn,
                    self.acc.id,
                    MsgIdScope::ConversationAuthor,
                    &dest[&conv],
                    t,
                    &local,
                    self.now,
                )? {
                    self.remove_messages(&[id])?;
                }
            }
        }
        if self.scope == MsgIdScope::AccountAuthor {
            let rows: Vec<(i64, String)> = self
                .conn
                .prepare_cached(AUTHORED_ROWS)?
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
                    self.merge_older(id, other)?;
                } else if crate::expiry::remembered(
                    self.conn,
                    self.acc.id,
                    MsgIdScope::AccountAuthor,
                    "",
                    t,
                    &local,
                    self.now,
                )? {
                    // No live twin, and the key it would get (the canonical author, the same id:
                    // the key is account-wide here) is one of a message that was REMOVED: the row
                    // goes, as in the per-sender passes above.
                    self.remove_messages(&[id])?;
                }
            }
        }
        self.conn.prepare_cached(REAUTHOR)?.execute((acc, f, t))?;
        self.conn
            .prepare_cached(REAUTHOR_REVISIONS)?
            .execute((acc, f, t))?;
        self.conn.prepare_cached(REDELETER)?.execute((acc, f, t))?;
        for c in touched {
            self.ch.timelines.insert(ConversationId(c));
            self.ch.recount_conv(ConversationId(c));
        }

        self.rewrite_user_rows(f, t)?;

        // Parked modifiers follow the canonical id.
        self.conn
            .prepare_cached(PENDING_RETARGET)?
            .execute((acc, f, t))?;
        self.conn
            .prepare_cached(PENDING_REKEY)?
            .execute((acc, &from_key, &into_key))?;
        // So do the keys of expired messages: a late copy under either identity stays dropped.
        self.conn
            .prepare_cached(EXPIRED_REAUTHOR)?
            .execute((acc, f, t))?;
        self.conn
            .prepare_cached(EXPIRED_REKEY)?
            .execute((acc, &from_key, &into_key))?;
        // And the cut of a clear by time: it can stand before its chat exists, so it follows the
        // key, not the conversation row.
        let moved_cut = self.move_cut(&from_key, &into_key)?;

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
                // The cut of `into`'s key may stand before this chat (it had none): the history
                // rows now under it are judged by it.
                self.clear_history_below_cut(&into_key)?;
            }
            // `from` has no chat: only its cut (if any) moved to `into`'s key, which may rise.
            (None, Some(_)) if moved_cut => self.clear_history_below_cut(&into_key)?,
            _ => {}
        }
        // A modifier that was sent in the chat under one identity may name a message that was
        // in the chat under the other until now: both are one conversation from here on.
        self.retry_pending(&into_key)?;
        // And a receipt that waited because its sender was no recipient of its message may
        // come from one now (looked at once for all merges of the transaction, in `finish`).
        self.ch.receipts_to_retry = true;
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
                // The alias's number verdict was about the alias; the canonical person has none
                // until the backend states one (NULL = hidden, the safe side; P1 F2).
                self.conn
                    .prepare_cached(
                        "UPDATE contacts SET user_id = ?3, number_shown = NULL
                         WHERE account_id = ?1 AND user_id = ?2",
                    )?
                    .execute((acc, from.as_str(), into.as_str()))?;
            }
            Some((mut t, t_avatar)) => {
                t.profile_name = t.profile_name.or(f.profile_name);
                t.system_name = t.system_name.or(f.system_name);
                t.nickname = t.nickname.or(f.nickname);
                // Trust moves WITH its key (P5 critic M3): the kept row's fingerprint and its own
                // trust, or the alias's pair when the kept row has no key.
                (t.trust, t.identity_fingerprint) = TrustLevel::merge(
                    (t.trust, t.identity_fingerprint),
                    (f.trust, f.identity_fingerprint),
                );
                t.unregistered_at = t.unregistered_at.or(f.unregistered_at);
                t.meta = t.meta.or(f.meta);
                // The person's profile text follows the same rule as the names: the canonical
                // row's own word wins, an empty one is filled (P1). The number verdict is NOT
                // carried over: it was stated for the alias's service id (a PNI has no profile
                // key, so it was "shown"); the canonical row keeps its own, or none (P1 F2).
                t.about = t.about.or(f.about);
                t.about_emoji = t.about_emoji.or(f.about_emoji);
                if t.identifiers.is_empty() {
                    t.identifiers = f.identifiers;
                }
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
            .prepare_cached(REACTIONS_OF)?
            .query_map((acc, from), |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })?
            .collect::<kourier_db::rusqlite::Result<_>>()?;
        for (mid, emoji, at, removed) in reactions {
            self.merge_reaction(MessageId(mid), into, &emoji, at, removed)?;
        }
        self.conn
            .prepare_cached(DELETE_REACTIONS_OF)?
            .execute((acc, from))?;

        let receipts: Vec<(i64, String, i64)> = self
            .conn
            .prepare_cached(RECEIPTS_OF)?
            .query_map((acc, from), |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<kourier_db::rusqlite::Result<_>>()?;
        let mut own_messages: Vec<i64> = Vec::new();
        for (mid, kind, at) in receipts {
            self.merge_receipt(MessageId(mid), into, &kind, at)?;
            own_messages.push(mid);
        }
        self.conn
            .prepare_cached(DELETE_RECEIPTS_OF)?
            .execute((acc, from))?;

        let sends: Vec<(i64, String, Option<String>)> = self
            .conn
            .prepare_cached(SENDS_OF)?
            .query_map((acc, from), |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<kourier_db::rusqlite::Result<_>>()?;
        for (mid, state, error) in sends {
            let state: RecipientState = sql::from_json(&state)?;
            self.merge_send_state(MessageId(mid), into, &state, error.as_deref())?;
            own_messages.push(mid);
        }
        self.conn
            .prepare_cached(DELETE_SENDS_OF)?
            .execute((acc, from))?;
        own_messages.sort_unstable();
        own_messages.dedup();
        for mid in own_messages {
            self.recompute_out(MessageId(mid))?;
        }
        Ok(())
    }

    /// Moves everything of `from` into `into` and deletes `from` (§3.3 steps 1-5). The read
    /// positions (KT-2) take the higher of the two: positions compare only within one id space, so a
    /// conversation that moved to another (a Telegram basic group upgraded to a supergroup) is NOT a
    /// plain merge. No path does that today.
    pub(crate) fn merge_conversation(
        &mut self,
        from: ConversationId,
        into: ConversationId,
    ) -> Result<()> {
        if from == into {
            return Ok(());
        }
        let acc = self.acc.id.0;
        // The key the rows have after the merge: `into`'s. A row of `from` whose message was
        // REMOVED under that key (a clear, a delete for me, the sweeper) and that has no twin in
        // `into` is removed with the merge, not moved: it must not come back, as a later copy of
        // it would not (it is remembered).
        let into_key = self.conv_key_of(into.0)?;
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
                let Some(other) = other else {
                    if crate::expiry::remembered(
                        self.conn,
                        self.acc.id,
                        MsgIdScope::Conversation,
                        &into_key,
                        "",
                        &local,
                        self.now,
                    )? {
                        self.remove_messages(&[id])?;
                    }
                    continue;
                };
                self.merge_older(id, other)?;
            }
        }
        // Per-sender keys (`ConversationAuthor`): the same author and id in both chats. A
        // peer's rows carry the canonical author in both (`identity_merged` re-authored them
        // before it got here), our own rows carry `''`.
        {
            let rows: Vec<(i64, String, String)> = self
                .conn
                .prepare_cached(CONV_ROWS_PER_SENDER)?
                .query_map([from.0], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<kourier_db::rusqlite::Result<_>>()?;
            for (id, author, local) in rows {
                let other: Option<i64> = self
                    .conn
                    .prepare_cached(SENDER_KEY_IN_CONV)?
                    .query_row((into.0, &author, &local), |r| r.get(0))
                    .optional()?;
                if let Some(other) = other {
                    self.merge_older(id, other)?;
                } else if crate::expiry::remembered(
                    self.conn,
                    self.acc.id,
                    MsgIdScope::ConversationAuthor,
                    &into_key,
                    &author,
                    &local,
                    self.now,
                )? {
                    self.remove_messages(&[id])?;
                }
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
        // The cuts of the two keys: the larger stands (a no-op when `identity_merged` moved it).
        self.move_cut(&keys.0, &keys.1)?;
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
                   read_seq = CASE
                     WHEN i.read_seq IS NULL THEN f.read_seq
                     WHEN f.read_seq IS NULL THEN i.read_seq
                     ELSE max(i.read_seq, f.read_seq) END,
                   peer_read_seq = CASE
                     WHEN i.peer_read_seq IS NULL THEN f.peer_read_seq
                     WHEN f.peer_read_seq IS NULL THEN i.peer_read_seq
                     ELSE max(i.peer_read_seq, f.peer_read_seq) END,
                   unread_hint = i.unread_hint + f.unread_hint,
                   -- The server's counts: both stated, their sum; one, that one. (Counts include
                   -- what `expiry_hidden` hides: that is the network's fact.)
                   unread_server = coalesce(i.unread_server + f.unread_server,
                     i.unread_server, f.unread_server),
                   mentions_server = coalesce(i.mentions_server + f.mentions_server,
                     i.mentions_server, f.mentions_server),
                   -- Deleted on another device (`ConversationCleared`) only when both sides
                   -- were: the messages of a side that is listed must not vanish from the
                   -- list by joining a chat that was deleted under the other identity.
                   hidden = min(i.hidden, f.hidden),
                   -- Protected when either side was: the messages of a protected side must not
                   -- show outside the chat by joining one that was not (S-P).
                   protected = max(i.protected, f.protected)
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
        // The merged read watermark covers messages of both sides, and so do the positions.
        let (watermark, read_seq, peer_read_seq): (Option<i64>, Option<i64>, Option<i64>) = self
            .conn
            .prepare_cached(
                "SELECT read_sent_at, read_seq, peer_read_seq FROM conversations WHERE id = ?1",
            )?
            .query_row([into.0], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
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
        if let Some(seq) = read_seq {
            self.read_rows_at(into, seq)?;
        }
        if let Some(seq) = peer_read_seq {
            self.floors_read_between(into, i64::MIN, seq)?;
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
        // 6. The cuts of the two chats are one now: the HISTORY rows of the merged chat at or
        // below it go, as they would have had the merge come before the history.
        self.clear_history_below_cut(&into_key)?;
        Ok(())
    }

    /// The conversation key (encoded) of conversation `id`.
    fn conv_key_of(&self, id: i64) -> Result<String> {
        Ok(self
            .conn
            .prepare_cached(CONV_KEY_OF)?
            .query_row([id], |r| r.get(0))?)
    }

    /// The cut of conversation key `from` becomes the cut of `into`, which keeps the larger of
    /// the two when it has one ([`CUTS_REKEY`]); `from` has none afterwards. True when `from`
    /// had one.
    fn move_cut(&mut self, from: &str, into: &str) -> Result<bool> {
        let acc = self.acc.id.0;
        let moved = self
            .conn
            .prepare_cached(CUTS_REKEY)?
            .execute((acc, from, into))?;
        self.conn.prepare_cached(CUTS_DROP)?.execute((acc, from))?;
        Ok(moved > 0)
    }

    /// Merges two rows of one key: the older one stays (§3.3 step 1), by its sort key, then
    /// its id. Rows that are gone (an earlier merge of the same pass took them) are skipped.
    fn merge_older(&mut self, a: i64, b: i64) -> Result<()> {
        let (Some(a), Some(b)) = (self.msg_row(MessageId(a))?, self.msg_row(MessageId(b))?) else {
            return Ok(());
        };
        let (keep, drop) = if (a.sort_key, a.id) <= (b.sort_key, b.id) {
            (a, b)
        } else {
            (b, a)
        };
        self.merge_rows(&keep, &drop)
    }

    /// Merges message `drop` into `keep` monotonically and deletes `drop`: two rows of one message,
    /// under one key (an identity merge) or under the allocated id and the server's (the re-key,
    /// [`super::rekey`], which makes a tombstone `drop` brings a whole one).
    pub(super) fn merge_rows(&mut self, keep: &MsgRow, drop: &MsgRow) -> Result<()> {
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
                     ELSE min(messages.expires_at, d.expires_at) END,
                   net_seq = coalesce(messages.net_seq, d.net_seq)
                 FROM (SELECT expiry_kind, expiry_timer_ms, expiry_started_at, expires_at, net_seq
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
        self.conn
            .prepare_cached(ALIASES_FOLLOW)?
            .execute((keep.id.0, drop.id.0))?;
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

#[cfg(test)]
mod tests {
    use kourier_backend_api::BackendEvent;
    use kourier_model::{
        ContactPatch, IdentityFingerprint, MsgIdScope, NetworkKind, Patch, TrustLevel, UserId,
    };

    fn uid(s: &str) -> UserId {
        UserId::parse(s).expect("user id")
    }

    fn trust_of(h: &crate::testing::IngestHarness, user: &str) -> (String, Option<Vec<u8>>) {
        h.connection()
            .query_row(
                "SELECT trust, fingerprint FROM contacts WHERE user_id = ?1",
                [user],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("contact row")
    }

    /// P5 critic M3: a merge moves the trust TOGETHER with the fingerprint it was given to.
    /// One way: the canonical row has its own never-verified key, the alias a Verified other
    /// key: the canonical row stays Unverified on its key (a `max` would put Verified on a key
    /// the user never compared). The other way: the canonical row has no key, the alias a
    /// Changed key: the pair moves (Changed, the alias's key).
    #[test]
    fn trust_moves_with_its_key_both_ways() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut h = crate::testing::IngestHarness::open(
            &dir.path().join("core.db"),
            &kourier_db::DbKey::from([3u8; 32]),
            NetworkKind::Fake,
            MsgIdScope::AccountAuthor,
            &[uid("aci:me")],
        )
        .expect("harness");
        let upsert = |h: &mut crate::testing::IngestHarness, user: &str, trust, key: Option<u8>| {
            let mut p = ContactPatch::new(uid(user));
            p.trust = Some(trust);
            if let Some(k) = key {
                p.identity_fingerprint = Patch::Set(IdentityFingerprint(vec![0x05, k]));
            }
            h.apply(vec![BackendEvent::ContactUpsert(p)])
                .expect("upsert");
        };
        let merge = |h: &mut crate::testing::IngestHarness, from: &str, into: &str| {
            h.apply(vec![BackendEvent::IdentityMerged {
                from: uid(from),
                into: uid(into),
            }])
            .expect("merge");
        };

        upsert(&mut h, "aci:ada", TrustLevel::Unverified, Some(1));
        upsert(&mut h, "pni:ada", TrustLevel::Verified, Some(2));
        merge(&mut h, "pni:ada", "aci:ada");
        let (trust, key) = trust_of(&h, "aci:ada");
        assert!(
            trust == "Unverified" && key == Some(vec![0x05, 1]),
            "the canonical key must keep its own trust, not the alias's Verified of another key"
        );

        upsert(&mut h, "aci:bob", TrustLevel::Verified, None);
        upsert(&mut h, "pni:bob", TrustLevel::Changed, Some(7));
        merge(&mut h, "pni:bob", "aci:bob");
        let (trust, key) = trust_of(&h, "aci:bob");
        assert!(
            trust == "Changed" && key == Some(vec![0x05, 7]),
            "a canonical row without a key must take the alias's trust together with its key"
        );
    }

    /// Every lookup of a merge starts from the alias's own rows (core.db schema 14): no walk
    /// of the account's messages (`messages_local`, `messages_key_account` on `account_id`
    /// alone, a list of every message id) and no scan of a table.
    #[test]
    fn a_merge_looks_only_at_the_rows_of_the_alias() {
        let dir = tempfile::tempdir().expect("tempdir");
        let h = crate::testing::IngestHarness::open(
            &dir.path().join("core.db"),
            &kourier_db::DbKey::from([3u8; 32]),
            NetworkKind::Fake,
            MsgIdScope::AccountAuthor,
            &[UserId::parse("aci:me").expect("user id")],
        )
        .expect("harness");
        let mut seen: Vec<String> = Vec::new();
        for sql in super::LOOKUPS {
            let steps: Vec<String> = h
                .connection()
                .prepare(&format!(
                    "EXPLAIN QUERY PLAN {}",
                    sql.replace("?1", "1")
                        .replace("?2", "'e164:+15550100'")
                        .replace("?3", "'d:e164:+15550100'")
                        .replace("?4", "'lid:7'")
                        .replace("?5", "0")
                ))
                .expect("prepare")
                .query_map([], |r| r.get::<_, String>(3))
                .expect("plan")
                .collect::<kourier_db::rusqlite::Result<_>>()
                .expect("rows");
            seen.extend(steps.iter().cloned());
            for step in &steps {
                assert!(
                    !step.starts_with("SCAN") || step == "SCAN CONSTANT ROW",
                    "{sql}\n{steps:#?}"
                );
                assert!(!step.contains("LIST SUBQUERY"), "{sql}\n{steps:#?}");
                let on_messages = step.starts_with("SEARCH messages ")
                    || step.starts_with("SEARCH m ")
                    || step.starts_with("SEARCH message_revisions ");
                if on_messages {
                    assert!(
                        step.contains("INTEGER PRIMARY KEY")
                            || step.contains("messages_by_author (account_id=? AND net_author=?)")
                            || step.contains("messages_deleted_by (account_id=? AND deleted_by=?)")
                            // The rows of ONE conversation (`merge_conversation`) and the
                            // partial key of per-sender ids: both start from a conversation.
                            || step.contains("messages_key_conv_author (conversation_id=?")
                            || step.contains("revisions_by_author (account_id=? AND net_author=?)"),
                        "a walk of the account's messages: {sql}\n{steps:#?}"
                    );
                }
                // The memory of removed messages (a row a merge would bring back) is looked up by
                // its local id on the indexes of migration 14.
                if sql.contains("FROM expired_messages") && sql.starts_with("SELECT EXISTS") {
                    assert!(
                        steps
                            .iter()
                            .any(|s| s.contains("expired_messages") && s.contains("net_local=?")),
                        "{sql}\n{steps:#?}"
                    );
                }
                for table in ["reactions", "receipts", "send_state", "r ", "s "] {
                    if step.starts_with(&format!("SEARCH {table}")) {
                        assert!(step.contains("(user_id=?)"), "{sql}\n{steps:#?}");
                    }
                }
                // Whatever the table: an index that is used for `account_id` alone walks all
                // of the account's rows of it (the keys of expired messages, parked modifiers).
                assert!(
                    !(step.starts_with("SEARCH ") && step.ends_with("(account_id=?)")),
                    "a walk of the account's rows: {sql}\n{steps:#?}"
                );
            }
        }
        // The test must SEE the lookups it guards: the ones on the keys of expired messages
        // and parked modifiers, each through its own index (a mutant without the four indexes
        // of migration 14 searches the old ones by `account_id` alone, which is refused above).
        for index in [
            "expired_messages_by_author (account_id=? AND net_author=?)",
            "expired_messages_by_conv (account_id=? AND conv_key=?)",
            "pending_by_author (account_id=? AND target_author=?)",
            "pending_by_conv (account_id=? AND conv_key=?)",
            "identity_alias_by_user (account_id=? AND user_id=?)",
            // Per-sender ids (K-B1): the rows of the alias, then the other copy by its key,
            // and the rows of one conversation by the same partial key.
            "messages_by_author (account_id=? AND net_author=?)",
            "messages_key_conv_author (conversation_id=? AND net_author=? AND net_local=?)",
            "messages_key_conv_author (conversation_id=?)",
            // The cut of a clear by time (K-B1): `ALIAS_OWNS`'s term, `CUTS_REKEY`'s source and
            // `CUTS_DROP` are lookups of its primary key.
            "conversation_cuts USING PRIMARY KEY (account_id=? AND conv_key=?)",
        ] {
            assert!(
                seen.iter().any(|step| step.contains(index)),
                "no lookup uses {index}: {seen:#?}"
            );
        }
    }
}
