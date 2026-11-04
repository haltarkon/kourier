//! Deletes for this account that are more or less than one message (§3.5.2): a conversation's
//! history cleared up to a point, the chat itself deleted, and one attachment of a message
//! deleted. All three are local to our account (nobody else is told), delete for good (rows
//! and files; the files are unlinked after the commit) and are idempotent. They come from two
//! sides with the same rules:
//!
//! - from another own device, as `ConversationCleared` and `AttachmentDeleted`
//!   ([`Apply::conversation_cleared`], [`Apply::attachment_deleted`]);
//! - from the user here ([`clear_conversation`], [`delete_attachment`]): applied in one
//!   transaction together with the durable command that tells our other devices
//!   (`pending_commands`, §4.8), so a kill between the two cannot leave one without the other.
//!
//! What a clear removed is remembered by key (`expired_messages`, the table the disappearing
//! sweeper fills, for the same [`REMEMBER`](crate::expiry::REMEMBER) time): ingest drops a
//! message with such a key, so a late copy (a redelivered envelope, a second copy under
//! another identity, a history import) does not bring a deleted message back.
//!
//! A clear names the newest messages its device had (its anchors) and may arrive before them:
//! a backend holds messages back (a group message waits for its group's state) while the
//! delete, a sync message of its own, passes. The clear then waits under each anchor that is
//! not here (`pending_modifiers`, two days like the other deletes for me) and is finished
//! when that message arrives: the message goes at once. When none of the anchors was here,
//! nothing could be deleted at first; what the conversation held at that moment goes with the
//! first anchor that arrives, which is what the clear would have deleted had its messages
//! come in order (they would have been the newest then). What arrived after the clear came
//! stays, as it stayed on the device that cleared.
//!
//! What a clear leaves is what still has to go out ([`UNSENT`], [`DELETE_QUEUED`]):
//!
//! - an own message nobody has yet: queued, being sent, or failed for everyone. No other
//!   device ever had it, and what the user wrote here is not lost to a delete made elsewhere;
//! - the row of a message whose delete for everyone still waits in the outbox: the row
//!   carries that delete, which must still reach the others. The clear hides it (the
//!   tombstone becomes one "for me", which nothing shows), so it is gone for the user at
//!   once and does not show as "deleted" again when the chat is used later.
//!
//! A message that reached some recipients and failed for the others goes with the clear like
//! any sent message: its outbox row is only kept for a "Retry" the user did not ask for, the
//! other devices have the message, and the device that cleared deleted it.
//!
//! A clear BY TIME (K-B1, `ConversationCleared.before_ms`; WhatsApp's range carries a time,
//! not anchors) and the user's own clear keep a CUT: `conversation_cuts`, the newest `sent_at`
//! the clear covers per canonical conversation KEY (not the row id: a clear can come before its
//! chat exists, as an app-state patch before the history that makes the chat). The cut is
//! stored FIRST, so a clear for a chat the core does not have yet still holds; it only rises
//! (a lower one never lowers it) and is never further ahead than the core's clock plus
//! [`CLEAR_CLOCK_TOLERANCE`] (a time later than that is read as that bound, at ingest and at
//! the user's clear: one wrong time would otherwise delete live messages and refuse the chat's
//! history for good; a few minutes of a clock that runs behind the network's are not wrong).
//!
//! A clear by time is IDEMPOTENT. The table keeps the CLAIM beside the cut (`claimed_ms`, the
//! time as the event stated it, before the clamp), and a delivery raises the cut, and removes
//! what is at or below it, only when its claim is LARGER than the stored claim. The same clear
//! delivered again (an app-state resync, a redelivery after a crash) changes nothing, so a live
//! message that arrived in between ("what arrived after the clear came stays") is not removed
//! by the replay, and a claim in the far future does not slide the cut to every later "now".
//! Anchors apply on every delivery, as before. Then, when the conversation is here, every
//! message at or below the cut goes, with the same exceptions as the anchors ([`UNSENT`],
//! [`DELETE_QUEUED`]) and the same memory of keys. What it REFUSES is the history that comes
//! later: a HISTORY row (a `HistoryBatch` row, K-B2's on-demand rows too) of the conversation at
//! or below the cut is dropped, once per batch ([`Apply::cut_of`], the batch arm of
//! `ingest/mod.rs`), and its key is REMEMBERED like the key of a removed row
//! ([`Apply::remember_refused`]), so the order of a history chunk and a clear does not decide
//! whether a later copy of that message shows. A chat cleared on the phone or here is not filled
//! up again by chunks that keep coming. LIVE rows and catch-up rows are never refused by a cut: a
//! live row's `sent_at` can be a sender's claim and a clock a day behind must not hide its live
//! messages ("what arrived after the clear came stays" holds as for anchors). Merges move the
//! cut with the alias (`merge::CUTS_REKEY`, keeping the larger of two, claim included) and
//! then clear the HISTORY rows of the merged chat at or below the joined cut
//! ([`Apply::clear_history_below_cut`]; live rows stay), or the order of a merge and the history
//! would decide what shows.
//!
//! A cut is kept when its chat is removed (`ConversationRemoved`, the chat deleted here or on
//! another device): a chat that is made again has the older history of the old one refused, as
//! on the phone, whose clear also stands for the chat it made. The table grows by one row per
//! cleared conversation key and nothing prunes it (a cut is a few dozen bytes). NOT done: a cut
//! inferred from Signal's anchors; a `remove` whose chat the core does not have yet stores the
//! cut but not the `remove` (the next upsert makes a listed empty chat: named for W6, whose
//! `deleteChatAction` maps here).
//!
//! The user's own clear stores a cut too: the newest `sent_at` that went (a notice's included),
//! or NOW when it removed nothing (a chat whose history has not arrived yet: the chunks that
//! come later are older than the clear and stay out). The durable command carries the times of
//! what went, split as the network's range splits them ([`Cleared`]).

use kourier_backend_api::{CLEAR_ANCHORS, CLEAR_CLOCK_TOLERANCE, DurableCommand};
use kourier_db::rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use kourier_model::{
    AccountId, AccountState, AttachmentId, Body, CmdId, ConvKey, ConversationFeatures,
    ConversationId, MessageId, MsgIdScope, MsgTarget, NetMsgId, Origin,
};
use serde::{Deserialize, Serialize};

use super::message::MsgRow;
use super::modifier::{CanonTarget, PENDING_SHORT_MS};
use super::{Apply, Changes};
use crate::actions::{self, Facts, Sending};
use crate::error::{CoreError, Refusal, Result};
use crate::sql;

/// `pending_modifiers.kind` of a clear that waits for one of its anchors.
pub(super) const CLEARED_KIND: &str = "ConversationCleared";

/// A clear that waits for one of its anchors (`pending_modifiers.payload`).
#[derive(Debug, Default, Serialize, Deserialize)]
struct ParkedClear {
    /// The newest timeline position `(sort_key, id)` the conversation had when the clear
    /// came, when none of its anchors was here then: everything up to it goes with the
    /// anchor. `None`: an anchor was here and what was to go went, or the conversation was
    /// empty.
    #[serde(default)]
    bound: Option<(i64, i64)>,
    /// The chat itself was deleted.
    #[serde(default)]
    remove: bool,
}

/// SQL: the own send of message `m` has reached nobody and is still in the outbox (queued,
/// being sent, being stored, or failed for every recipient). `send_state.state` is the JSON
/// of a `RecipientState`, whose `Sent` is an object ([`tests::sent_is_matched_by_its_json`]).
const UNSENT: &str = "EXISTS (SELECT 1 FROM outbox o WHERE o.message_id = m.id
    AND o.kind = 'Message' AND NOT (o.state = 'Failed' AND EXISTS (
      SELECT 1 FROM send_state s WHERE s.message_id = m.id AND s.state LIKE '{\"Sent\"%')))";

/// SQL: a delete for everyone of message `m` still waits in the outbox.
const DELETE_QUEUED: &str =
    "EXISTS (SELECT 1 FROM outbox o WHERE o.message_id = m.id AND o.kind = 'Delete')";

/// SQL: the cut of the conversation key `?2` of account `?1` (one primary-key lookup; plan test
/// `tests::the_cut_of_a_history_row_is_one_key_lookup`).
pub(crate) const CUT_OF: &str =
    "SELECT before_ms FROM conversation_cuts WHERE account_id = ?1 AND conv_key = ?2";

/// SQL: stores the claim `?4` (the time as the clear stated it) and its clamped cut `?3` for a
/// conversation key. Only a LARGER claim than the stored one changes the row (the cut rises, never
/// lowers; the claim is taken): a clear delivered again changes nothing, and `execute` says
/// whether this one did (1 row) or not (0).
pub(crate) const STORE_CUT: &str =
    "INSERT INTO conversation_cuts(account_id, conv_key, before_ms, claimed_ms)
 VALUES (?1, ?2, ?3, ?4)
 ON CONFLICT(account_id, conv_key) DO UPDATE SET
   before_ms = max(before_ms, excluded.before_ms), claimed_ms = excluded.claimed_ms
 WHERE excluded.claimed_ms > claimed_ms";

/// SQL: the user's OWN clear here (a clear made in Kourier, at the core's clock): the local cut
/// `?3` rises to at least this clear's time, whatever claim is stored. [`STORE_CUT`] does nothing
/// when the stored claim is the larger (a far-future claim of the phone, clamped to the clock of
/// its day): the user's clear, hours later, would leave the cut where the old clamp put it and the
/// history between the two would come back. The claim only rises to `?3` (`max`, never lowered), so
/// a replay of the phone's claim still changes nothing.
pub(crate) const STORE_OWN_CUT: &str =
    "INSERT INTO conversation_cuts(account_id, conv_key, before_ms, claimed_ms)
 VALUES (?1, ?2, ?3, ?3)
 ON CONFLICT(account_id, conv_key) DO UPDATE SET
   before_ms = max(before_ms, excluded.before_ms),
   claimed_ms = max(claimed_ms, excluded.claimed_ms)";

/// The range of a clear by time: every message at or below the cut `?2`.
const BY_TIME: &str = "m.sent_at <= ?2";

/// The range of the clean-up after a merge joined two cuts: the HISTORY rows at or below the
/// joined cut `?2` (live rows stay, [`Apply::clear_history_below_cut`]).
const HISTORY_BY_TIME: &str = "m.history = 1 AND m.sent_at <= ?2";

/// SQL: remembers the key of a history row the cut refused, as [`Apply::remove_messages`]
/// remembers a removed one: `?1` account, `?2` scope, `?3` conversation key, `?4` author, `?5`
/// local id, `?6` now, `?7` the oldest time that still counts
/// ([`REMEMBER`](crate::expiry::REMEMBER)). Once per key: a refused chunk that is delivered again
/// adds nothing (the table has no unique key, and a replay must not grow it).
pub(crate) const REMEMBER_REFUSED: &str =
    "INSERT INTO expired_messages(account_id, net_scope, conv_key,
   net_author, net_local, expired_at)
 SELECT ?1, ?2, ?3, ?4, ?5, ?6
 WHERE NOT EXISTS (SELECT 1 FROM expired_messages WHERE account_id = ?1 AND net_local = ?5
   AND net_scope = ?2 AND conv_key = ?3 AND net_author = ?4 AND expired_at >= ?7)";

/// What clearing a set of messages did.
#[derive(Clone, Copy, Debug, Default)]
struct Cleared {
    /// Something changed for the user.
    changed: bool,
    /// The newest `sent_at` among the rows that went (or were hidden) that a person sent: not
    /// `System`, not `Placeholder` (the filter of [`Apply::anchors`]), when any did.
    newest_message_at: Option<i64>,
    /// The newest `sent_at` among the other rows that went (or were hidden), when any did.
    newest_system_at: Option<i64>,
}

/// What a delete made on this device did: the invalidations, and the durable commands that
/// tell our other devices (already stored in `pending_commands`).
#[derive(Debug)]
pub(crate) struct LocalDelete {
    pub account: AccountId,
    pub changes: Changes,
    pub commands: Vec<(CmdId, DurableCommand)>,
}

/// SQL: the messages of the conversation `?1` that `range` selects and a clear removes
/// (`id`, `sent_at`, `body_kind`): not what still has to go out.
fn select_gone(range: &str) -> String {
    format!(
        "SELECT m.id, m.sent_at, m.body_kind FROM messages m WHERE m.conversation_id = ?1 AND {range}
           AND NOT {UNSENT} AND NOT {DELETE_QUEUED}"
    )
}

/// SQL: the rows of a message whose delete for everyone still waits, that `range` selects and a
/// clear hides (same columns).
fn select_hidden(range: &str) -> String {
    format!(
        "SELECT m.id, m.sent_at, m.body_kind FROM messages m WHERE m.conversation_id = ?1 AND {range}
           AND {DELETE_QUEUED} AND (m.deleted IS NULL OR m.deleted <> 'ForMe')"
    )
}

thread_local! {
    /// How many times this thread read and parsed a stored message body
    /// ([`Apply::body_of`]): a probe for the tests that a loop over the attachments of one
    /// message parses its body once (`testing::body_reads`).
    static BODY_READS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// The count of [`BODY_READS`] on this thread.
pub(crate) fn body_reads() -> u64 {
    BODY_READS.with(std::cell::Cell::get)
}

impl Apply<'_> {
    /// `ConversationCleared`: every message of the conversation up to the newest of `up_to`
    /// that is here goes, in timeline order; an own message nobody has yet stays (the other
    /// device never had it, and what the user wrote here is not lost to a delete made
    /// elsewhere), and the row of a message whose delete still waits in the outbox stays
    /// hidden (it carries that delete, which must still reach the others). With `remove`, a
    /// conversation
    /// left without a message a person sent loses its notices too and is hidden from the
    /// conversation list until it is used again. An anchor that is not here yet is waited
    /// for (module docs). With `before_ms` every message at or below that time goes too, and
    /// the time is kept as the conversation's cut (module docs).
    pub(crate) fn conversation_cleared(
        &mut self,
        key: &ConvKey,
        up_to: &[MsgTarget],
        remove: bool,
        before_ms: Option<i64>,
    ) -> Result<()> {
        let key = self.canon_conv(key)?;
        let encoded = key.encode();
        // The cut first: a clear that comes before its chat holds for the chat (the early
        // return below), and a kill after this line leaves the clear's other half to the
        // event's replay (the transaction is the event's). A claim that is not larger than the
        // stored one (a replay of the clear) raises nothing and removes nothing by time.
        let cut = match before_ms {
            Some(claim) => {
                let cut = self.clamp_cut(claim);
                self.store_cut(&encoded, claim, cut)?.then_some(cut)
            }
            None => None,
        };
        let conv = sql::conversation_id(self.conn, self.acc.id, &key)?;
        // The newest anchor that is a message of this conversation. A reference that resolves
        // to a message of another chat (account-wide ids) is no anchor here; like one that
        // resolves to nothing, it may still come into this conversation.
        let mut newest: Option<(i64, i64)> = None;
        let mut missing: Vec<CanonTarget> = Vec::new();
        for t in up_to {
            let mut t = t.clone();
            t.conv = Some(key.clone());
            let canon = self.canon_target(&t)?;
            match (conv, self.resolve_target(&canon)?) {
                (Some(conv), Some(row)) if row.conv == conv => {
                    newest = newest.max(Some((row.sort_key, row.id.0)));
                }
                _ => missing.push(canon),
            }
        }
        let bound = match (conv, newest) {
            (Some(conv), None) => self.top_of(conv)?,
            _ => None,
        };
        for anchor in &missing {
            // An anchor a clear removed before is not coming back: a replay of this event
            // must not wait for it again.
            if !self.was_removed(&encoded, anchor)? {
                self.park_clear(anchor, bound, remove)?;
            }
        }
        let Some(conv) = conv else {
            return Ok(());
        };
        let mut changed = match newest {
            Some(position) => self.clear_up_to(conv, position)?.changed,
            None => false,
        };
        if let Some(cut) = cut {
            changed |= self.clear_where(BY_TIME, (conv.0, cut))?.changed;
        }
        self.after_clear(conv, remove, changed)
    }

    /// A time further ahead than the core's clock plus [`CLEAR_CLOCK_TOLERANCE`] is read as that
    /// bound: a clear cannot name the far future, and a cut only rises, so one wrong time (a
    /// phone's clock, a peer's claim) would otherwise be for ever. The network's clock may run
    /// ahead of this computer's by minutes, which is not wrong.
    fn clamp_cut(&self, claim: i64) -> i64 {
        claim.min(
            self.now
                .saturating_add(crate::outbox::ms(CLEAR_CLOCK_TOLERANCE)),
        )
    }

    /// The cut of the conversation with canonical key `conv_key` (encoded), when it has one.
    pub(super) fn cut_of(&self, conv_key: &str) -> Result<Option<i64>> {
        Ok(self
            .conn
            .prepare_cached(CUT_OF)?
            .query_row((self.acc.id.0, conv_key), |r| r.get(0))
            .optional()?)
    }

    /// Stores the claim of a clear and its cut (already clamped) for the conversation with
    /// canonical key `conv_key` ([`STORE_CUT`]). True when the claim was larger than the stored
    /// one, i.e. the clear is news; false for a replay of a clear that was applied.
    fn store_cut(&self, conv_key: &str, claim: i64, cut: i64) -> Result<bool> {
        let changed =
            self.conn
                .prepare_cached(STORE_CUT)?
                .execute((self.acc.id.0, conv_key, cut, claim))?;
        Ok(changed > 0)
    }

    /// Stores the cut of the user's own clear ([`STORE_OWN_CUT`]): always at least `cut`, also when
    /// a larger claim of the network is stored (the claim stays).
    fn store_own_cut(&self, conv_key: &str, cut: i64) -> Result<()> {
        self.conn
            .prepare_cached(STORE_OWN_CUT)?
            .execute((self.acc.id.0, conv_key, cut))?;
        Ok(())
    }

    /// A history row the cut of its chat refuses: its key is remembered, as the key of a row a
    /// clear removed is ([`Self::remove_messages`]), so a later copy of the message (a live
    /// one, one under another identity) is dropped whichever came first, the history chunk or
    /// the clear. The key is the one `message` would look up: the canonical chat, the author after
    /// identity resolution (`''` for our own), the local id, and the call lines' own scope.
    pub(super) fn remember_refused(&self, m: &kourier_backend_api::IncomingMessage) -> Result<()> {
        let key = self.canon_conv(&m.conv)?;
        let (scope, author) = if m.body.call_line().is_some() {
            (MsgIdScope::Conversation, String::new())
        } else {
            let author = match &m.origin {
                Origin::Remote { author, .. } => {
                    let a = self.resolve_user(author)?;
                    if self.is_own(&a) {
                        String::new()
                    } else {
                        a.as_str().to_owned()
                    }
                }
                Origin::OwnThisDevice | Origin::OwnOtherDevice { .. } => String::new(),
            };
            (self.scope, author)
        };
        self.conn.prepare_cached(REMEMBER_REFUSED)?.execute((
            self.acc.id.0,
            scope.db_code(),
            key.encode(),
            author,
            &m.local,
            self.now,
            self.now
                .saturating_sub(crate::outbox::ms(crate::expiry::REMEMBER)),
        ))?;
        Ok(())
    }

    /// After a merge joined the cuts of two conversation keys: the HISTORY rows of the
    /// conversation with key `conv_key` at or below its cut go (their keys remembered), as if the
    /// merge had come first and the chunks after it. LIVE rows stay ("what arrived after the clear
    /// came stays"). One primary-key look-up of the cut when the conversation has none; with one,
    /// a pass over the conversation's rows, which a merge of a cleared chat makes once.
    pub(super) fn clear_history_below_cut(&mut self, conv_key: &str) -> Result<()> {
        let Some(cut) = self.cut_of(conv_key)? else {
            return Ok(());
        };
        let Some(conv) = self.conversation_by_key(conv_key)? else {
            return Ok(());
        };
        let changed = self.clear_where(HISTORY_BY_TIME, (conv.0, cut))?.changed;
        self.after_clear(conv, false, changed)
    }

    /// The conversation of this account with canonical key `conv_key` (encoded).
    fn conversation_by_key(&self, conv_key: &str) -> Result<Option<ConversationId>> {
        Ok(self
            .conn
            .prepare_cached("SELECT id FROM conversations WHERE account_id = ?1 AND conv_key = ?2")?
            .query_row((self.acc.id.0, conv_key), |r| r.get(0))
            .optional()?
            .map(ConversationId))
    }

    /// The anchor `row` of a clear that waited for it arrived (`payload`: the
    /// [`ParkedClear`]): it goes, and with it what the clear could not delete when it came
    /// because no anchor was here.
    pub(super) fn late_clear(&mut self, row: &MsgRow, payload: &str) -> Result<()> {
        let parked: ParkedClear = sql::from_json(payload)?;
        let (bound_key, bound_id) = parked.bound.unwrap_or((i64::MIN, i64::MIN));
        let range = "(m.id = ?2 OR m.sort_key < ?3 OR (m.sort_key = ?3 AND m.id <= ?4))";
        let changed = self
            .clear_where(range, (row.conv.0, row.id.0, bound_key, bound_id))?
            .changed;
        self.after_clear(row.conv, parked.remove, changed)
    }

    /// The newest timeline position of `conv`, when it has a message.
    fn top_of(&self, conv: ConversationId) -> Result<Option<(i64, i64)>> {
        Ok(self
            .conn
            .prepare_cached(
                "SELECT sort_key, id FROM messages WHERE conversation_id = ?1
                 ORDER BY sort_key DESC, id DESC LIMIT 1",
            )?
            .query_row([conv.0], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?)
    }

    /// Whether a clear (or the sweeper) removed the message `anchor` names from the
    /// conversation `conv_key` before.
    fn was_removed(&self, conv_key: &str, anchor: &CanonTarget) -> Result<bool> {
        let author = match (&anchor.author, self.scope) {
            (Some(a), _) => a.as_str(),
            // Without an author a key that holds one (account-wide, or per sender in a chat)
            // cannot be looked up.
            (None, MsgIdScope::AccountAuthor | MsgIdScope::ConversationAuthor) => {
                return Ok(false);
            }
            (None, MsgIdScope::Conversation) => "",
        };
        for local in &anchor.locals {
            if crate::expiry::remembered(
                self.conn,
                self.acc.id,
                self.scope,
                conv_key,
                author,
                local,
                self.now,
            )? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Lets a clear wait for `anchor`. A replay of the clear waits once: what makes two
    /// waiting clears the same is the anchor and `remove`, not the bound, which is the one
    /// of the first time the clear came.
    fn park_clear(
        &mut self,
        anchor: &CanonTarget,
        bound: Option<(i64, i64)>,
        remove: bool,
    ) -> Result<()> {
        let payload = sql::to_json(&ParkedClear { bound, remove })?;
        let identity = if remove { "remove" } else { "clear" };
        self.park_raw(anchor, CLEARED_KIND, &payload, identity, PENDING_SHORT_MS)
    }

    /// Deletes the messages of `conv` up to `position` in timeline order, apart from what
    /// still has to go out (module docs).
    fn clear_up_to(&mut self, conv: ConversationId, position: (i64, i64)) -> Result<Cleared> {
        let range = "(m.sort_key < ?2 OR (m.sort_key = ?2 AND m.id <= ?3))";
        self.clear_where(range, (conv.0, position.0, position.1))
    }

    /// Clears the messages of the conversation `?1` that `range` (SQL over `m`, the other
    /// parameters) selects: they go, apart from an own message nobody has yet, which stays,
    /// and a message whose delete for everyone still waits, whose row stays hidden. Says
    /// whether anything changed for the user and the newest `sent_at` of what went.
    fn clear_where(
        &mut self,
        range: &str,
        params: impl kourier_db::rusqlite::Params + Copy,
    ) -> Result<Cleared> {
        let gone: Vec<(i64, i64, String)> = self
            .conn
            .prepare_cached(&select_gone(range))?
            .query_map(params, |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<kourier_db::rusqlite::Result<_>>()?;
        // The row of a message whose delete still has to reach the others carries that
        // delete (a queued delete is always sent, §4.8 step 7: the user asked for the
        // message to be taken from everybody, and a delete for me must not leave it with
        // them). For the user it is gone like the rest: nothing shows a tombstone "for me".
        let hidden: Vec<(i64, i64, String)> = self
            .conn
            .prepare_cached(&select_hidden(range))?
            .query_map(params, |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<kourier_db::rusqlite::Result<_>>()?;
        for (id, _, _) in &hidden {
            let Some(row) = self.msg_row(MessageId(*id))? else {
                continue;
            };
            self.tombstone(&row, "ForMe", Some(""))?;
        }
        let ids: Vec<i64> = gone.iter().map(|(id, _, _)| *id).collect();
        self.remove_messages(&ids)?;
        let newest = |system: bool| {
            gone.iter()
                .chain(&hidden)
                .filter(|(_, _, kind)| (kind == "System" || kind == "Placeholder") == system)
                .map(|(_, at, _)| *at)
                .max()
        };
        Ok(Cleared {
            changed: !gone.is_empty() || !hidden.is_empty(),
            newest_message_at: newest(false),
            newest_system_at: newest(true),
        })
    }

    /// The end of every clear of `conv`: with `remove`, a conversation without a message a
    /// person sent loses its notices and leaves the list; then the invalidations (`changed`:
    /// messages went).
    fn after_clear(&mut self, conv: ConversationId, remove: bool, mut changed: bool) -> Result<()> {
        if remove {
            let left: bool = self
                .conn
                .prepare_cached(
                    "SELECT EXISTS(SELECT 1 FROM messages WHERE conversation_id = ?1
                       AND body_kind <> 'System' AND (deleted IS NULL OR deleted <> 'ForMe'))",
                )?
                .query_row([conv.0], |r| r.get(0))?;
            if !left {
                // A hidden tombstone whose delete still waits in the outbox stays as a row
                // (nothing shows it), so the delete is sent.
                let rest: Vec<i64> = self
                    .conn
                    .prepare_cached(&format!(
                        "SELECT m.id FROM messages m WHERE m.conversation_id = ?1
                           AND NOT {DELETE_QUEUED}"
                    ))?
                    .query_map([conv.0], |r| r.get(0))?
                    .collect::<kourier_db::rusqlite::Result<_>>()?;
                changed |= !rest.is_empty();
                self.remove_messages(&rest)?;
                let hidden = self
                    .conn
                    .prepare_cached(
                        "UPDATE conversations SET hidden = 1, marked_unread = 0, unread_hint = 0,
                           unread_server = CASE WHEN unread_server IS NULL THEN NULL ELSE 0 END,
                           mentions_server = CASE WHEN mentions_server IS NULL THEN NULL ELSE 0 END
                         WHERE id = ?1 AND hidden = 0",
                    )?
                    .execute([conv.0])?;
                if hidden > 0 {
                    // Counted now: `finish` skips conversations that left the list.
                    self.recount(conv)?;
                    self.ch.conversations.remove(&conv);
                    self.ch.messages.remove(&conv);
                    self.ch.removed.insert(conv);
                    self.ch.timelines.insert(conv);
                    return Ok(());
                }
            }
        }
        if changed {
            self.ch.conversations.insert(conv);
            self.ch.timelines.insert(conv);
            self.ch.recount_conv(conv);
        }
        Ok(())
    }

    /// Deletes message rows for good and remembers their keys, so a later copy of one is
    /// dropped: their attachments and files first (unlinked after the commit), then the rows;
    /// reactions, receipts, revisions, send states and outbox rows go with them.
    pub(super) fn remove_messages(&mut self, ids: &[i64]) -> Result<()> {
        for id in ids {
            self.conn
                .prepare_cached(
                    "INSERT INTO expired_messages(account_id, net_scope, conv_key, net_author,
                       net_local, expired_at)
                     SELECT m.account_id, m.net_scope, c.conv_key, m.net_author, m.net_local, ?2
                     FROM messages m JOIN conversations c ON c.id = m.conversation_id
                     WHERE m.id = ?1",
                )?
                .execute((id, self.now))?;
            self.delete_attachments(MessageId(*id))?;
            self.conn
                .prepare_cached("DELETE FROM messages WHERE id = ?1")?
                .execute([id])?;
        }
        // Edits, reactions and deletes that waited for them went along.
        self.ch.outbox |= !ids.is_empty();
        Ok(())
    }

    /// A message was stored or queued in `conv`: a conversation hidden by a delete is listed
    /// again.
    pub(crate) fn unhide(&mut self, conv: ConversationId) -> Result<()> {
        let shown = self
            .conn
            .prepare_cached("UPDATE conversations SET hidden = 0 WHERE id = ?1 AND hidden = 1")?
            .execute([conv.0])?;
        if shown > 0 {
            self.ch.removed.remove(&conv);
            self.ch.conversations.insert(conv);
        }
        Ok(())
    }

    /// `AttachmentDeleted` for a message that is here: `net_ids` are in order of priority
    /// (Signal: client UUID, digest, plaintext hash, as Desktop's `deleteForMe` tries them),
    /// and the first NAME that any attachment of `row` has decides: that attachment goes,
    /// with its files. Priority runs over ALL attachments, not attachment by attachment: two
    /// copies of one file share a plaintext hash, and the phone's delete of the second must
    /// not take the first because its hash comes first in the message. Unknown names change
    /// nothing, and nothing is left of a tombstone to delete from.
    pub(super) fn attachment_deleted(&mut self, row: &MsgRow, net_ids: &[String]) -> Result<()> {
        if row.deleted.is_some() {
            return Ok(());
        }
        let body = self.body_of(row.id)?;
        let at = net_ids.iter().find_map(|name| match &body {
            Body::Media { attachments, .. } => {
                attachments.iter().position(|a| a.net_ids.contains(name))
            }
            Body::Sticker(a) => a.net_ids.contains(name).then_some(0),
            _ => None,
        });
        match at {
            Some(at) => self.remove_attachment(row, body, at),
            None => Ok(()),
        }
    }

    /// The stored body of a message (its latest revision).
    pub(super) fn body_of(&self, id: MessageId) -> Result<Body> {
        BODY_READS.with(|n| n.set(n.get() + 1));
        let body: String = self
            .conn
            .prepare_cached("SELECT body FROM messages WHERE id = ?1")?
            .query_row([id.0], |r| r.get(0))?;
        sql::from_json(&body)
    }

    /// Removes attachment number `at` of `row` (whose stored body is `body`) with its files.
    /// The attachments behind it move up, in the rows and in every stored body (the
    /// message's and its revisions'). A message left with neither an attachment nor a text
    /// becomes a delete-for-me tombstone.
    fn remove_attachment(&mut self, row: &MsgRow, body: Body, at: usize) -> Result<()> {
        if matches!(body, Body::Sticker(_)) {
            // The sticker is the message.
            return self.tombstone(row, "ForMe", Some(""));
        }
        let idx = i64::try_from(at).unwrap_or(i64::MAX);
        // The row, its files and its download target, then the rows behind it (ascending, so
        // `idx` stays unique).
        type AttRow = (i64, Option<i64>, Option<i64>);
        let att: Option<AttRow> = self
            .conn
            .prepare_cached(
                "SELECT id, file_id, thumb_file_id FROM attachments
                 WHERE message_id = ?1 AND idx = ?2",
            )?
            .query_row((row.id.0, idx), |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .optional()?;
        if let Some((id, file, thumb)) = att {
            // A download of it that is still in flight leaves no file behind.
            self.orphan_download(id)?;
            self.conn
                .prepare_cached("DELETE FROM attachments WHERE id = ?1")?
                .execute([id])?;
            for f in [file, thumb].into_iter().flatten() {
                self.delete_file(f)?;
            }
            self.ch.media.insert(AttachmentId(id));
            self.ch.media_gone.insert(AttachmentId(id));
        }
        let later: Vec<i64> = self
            .conn
            .prepare_cached(
                "SELECT id FROM attachments WHERE message_id = ?1 AND idx > ?2 ORDER BY idx",
            )?
            .query_map((row.id.0, idx), |r| r.get(0))?
            .collect::<kourier_db::rusqlite::Result<_>>()?;
        for id in later {
            self.conn
                .prepare_cached("UPDATE attachments SET idx = idx - 1 WHERE id = ?1")?
                .execute([id])?;
        }
        let rest = crate::attachments::derive::without_attachment(body, at);
        if matches!(&rest, Body::Text(t) if t.body.trim().is_empty()) {
            return self.tombstone(row, "ForMe", Some(""));
        }
        let revisions: Vec<(i64, Option<String>)> = self
            .conn
            .prepare_cached("SELECT rowid, body FROM message_revisions WHERE message_id = ?1")?
            .query_map([row.id.0], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<kourier_db::rusqlite::Result<_>>()?;
        if revisions.is_empty() {
            self.conn
                .prepare_cached(
                    "UPDATE messages SET body = ?2, body_kind = ?3, preview = ?4 WHERE id = ?1",
                )?
                .execute(params![
                    row.id.0,
                    sql::to_json(&rest)?,
                    rest.kind_name(),
                    super::preview(&rest)
                ])?;
        } else {
            for (rowid, body) in revisions {
                let Some(body) = body else { continue };
                let body =
                    crate::attachments::derive::without_attachment(sql::from_json(&body)?, at);
                self.conn
                    .prepare_cached("UPDATE message_revisions SET body = ?2 WHERE rowid = ?1")?
                    .execute((rowid, sql::to_json(&body)?))?;
            }
            // The latest revision is the row's body again.
            self.reindex_revisions(row.id)?;
        }
        self.ch.message(row.conv, row.id);
        Ok(())
    }

    /// The newest messages of `conv` a person sent that our other devices can have, newest
    /// first, at most [`CLEAR_ANCHORS`]: the anchors of a clear made here. Not a notice or a
    /// stand-in (no device addresses those), not a message hidden by a delete for me (the
    /// other devices deleted it with us), not an own message nobody has yet ([`UNSENT`]: no
    /// other device has it; one that reached some recipients is with our other devices and
    /// can be named). `lasting`: only messages without a disappearing timer.
    fn anchors(
        &self,
        conv: ConversationId,
        key: &ConvKey,
        lasting: bool,
    ) -> Result<Vec<MsgTarget>> {
        let rows: Vec<(String, String)> = self
            .conn
            .prepare_cached(&format!(
                "SELECT m.net_author, m.net_local FROM messages m WHERE m.conversation_id = ?1
                   AND m.body_kind NOT IN ('System', 'Placeholder')
                   AND (m.deleted IS NULL OR m.deleted <> 'ForMe')
                   AND (?2 = 0 OR m.expiry_timer_ms IS NULL)
                   AND NOT {UNSENT}
                 ORDER BY m.sort_key DESC, m.id DESC LIMIT ?3"
            ))?
            .query_map(
                (conv.0, lasting, i64::try_from(CLEAR_ANCHORS).unwrap_or(5)),
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?
            .collect::<kourier_db::rusqlite::Result<_>>()?;
        rows.into_iter()
            .map(|(author, local)| {
                let author = (!author.is_empty())
                    .then(|| sql::user(&author))
                    .transpose()?;
                let id = NetMsgId {
                    author,
                    local: local.as_str().into(),
                };
                Ok(MsgTarget::to_id(Some(key.clone()), &id))
            })
            .collect()
    }

    /// Stores a durable command of this account (`pending_commands`, §4.8).
    pub(super) fn queue_command(&self, cmd: DurableCommand) -> Result<(CmdId, DurableCommand)> {
        self.conn
            .prepare_cached(
                "INSERT INTO pending_commands(account_id, command, created_at) VALUES (?1, ?2, ?3)",
            )?
            .execute((self.acc.id.0, sql::to_json(&cmd)?, self.now))?;
        Ok((CmdId(self.conn.last_insert_rowid()), cmd))
    }

    /// Whether a delete for me made here is told to our other devices: the account is
    /// linked, and the network syncs such deletes (unknown features: the backend decides).
    fn syncs_deletes(&self, features: Option<&ConversationFeatures>) -> bool {
        self.acc.state == AccountState::Active && features.is_none_or(|f| f.delete_for_me)
    }
}

/// The conversation's account, key and features.
pub(super) fn conversation(
    conn: &Connection,
    conv: ConversationId,
) -> Result<Option<(AccountId, ConvKey, Option<ConversationFeatures>)>> {
    let row: Option<(i64, String, Option<String>)> = conn
        .prepare_cached("SELECT account_id, conv_key, features FROM conversations WHERE id = ?1")?
        .query_row([conv.0], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .optional()?;
    row.map(|(account, key, features)| {
        Ok((
            AccountId(account),
            sql::conv_key(&key)?,
            sql::from_json_opt(features)?,
        ))
    })
    .transpose()
}

/// `UiCommand::ClearConversation` and, with `remove`, `UiCommand::DeleteConversation`: one
/// transaction deletes every message of `conv` (apart from what still has to go out, as for
/// the same delete made on another device), remembers their keys and stores the durable
/// command that tells our other devices, which names the newest messages the conversation
/// had. With `remove` the draft goes too and a conversation that is left without a message a
/// person sent leaves the list. The newest `sent_at` that went (at most the clock plus the
/// tolerance), or now when nothing went, is kept as the conversation's cut, and it always raises
/// the LOCAL cut to at least that time, also when a larger claim of the network is stored
/// ([`STORE_OWN_CUT`]); the command carries the times of what went as `before_ms` and
/// `system_before_ms` (module docs).
///
/// No command is stored for an account that is not linked, on a network that does not sync
/// deletes for me, or for a clear that had nothing to name (there is nothing the other devices
/// could delete up to); a deleted conversation is told even when it was empty.
pub(crate) fn clear_conversation(
    conn: &mut Connection,
    scope: MsgIdScope,
    conv: ConversationId,
    remove: bool,
    now: i64,
) -> Result<LocalDelete> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let (account, key, features) =
        conversation(&tx, conv)?.ok_or(CoreError::UnknownConversation(conv))?;
    let mut apply = Apply::begin(&tx, account, scope, now)?;
    // What the other devices delete up to: read before anything goes.
    let newest = apply.anchors(conv, &key, false)?;
    let newest_non_expiring = apply.anchors(conv, &key, true)?;
    let cleared = apply.clear_up_to(conv, (i64::MAX, i64::MAX))?;
    let changed = cleared.changed;
    // The times of what went, never after the clock plus the tolerance (one wrong `sent_at` of a
    // peer would refuse the chat's history for good): of the rows a person sent and of the
    // others (a notice, a stand-in), as the network's range keeps them apart (WhatsApp, W6).
    // The cut is the larger of both, and NOW when nothing went (a chat whose history has not
    // arrived yet: the chunks that come later are older than this clear). Kept for every
    // network, so a chat cleared here stays cleared while history chunks still arrive. The
    // durable commands carry the two times, not the cut: `None` when nothing of that kind went.
    let before_ms = cleared.newest_message_at.map(|at| apply.clamp_cut(at));
    let system_before_ms = cleared.newest_system_at.map(|at| apply.clamp_cut(at));
    let cut = before_ms.max(system_before_ms).unwrap_or(apply.now);
    apply.store_own_cut(&key.encode(), cut)?;
    if remove {
        let cleared = tx
            .prepare_cached(
                "UPDATE conversations SET draft = NULL WHERE id = ?1 AND draft IS NOT NULL",
            )?
            .execute([conv.0])?;
        if cleared > 0 {
            apply.ch.conversations.insert(conv);
        }
    }
    apply.after_clear(conv, remove, changed)?;
    let mut commands = Vec::new();
    if apply.syncs_deletes(features.as_ref()) && (remove || !newest.is_empty()) {
        commands.push(apply.queue_command(DurableCommand::ClearConversation {
            conv: key,
            newest,
            newest_non_expiring,
            remove,
            before_ms,
            system_before_ms,
        })?);
    }
    apply.finish()?;
    let changes = std::mem::take(&mut apply.ch);
    drop(apply);
    tx.commit()?;
    Ok(LocalDelete {
        account,
        changes,
        commands,
    })
}

/// The account of an attachment's message, for a UI command.
pub(crate) fn attachment_account(
    conn: &Connection,
    attachment: AttachmentId,
) -> Result<Option<(AccountId, String)>> {
    Ok(conn
        .prepare_cached(
            "SELECT m.account_id, a.network FROM attachments x
             JOIN messages m ON m.id = x.message_id JOIN accounts a ON a.id = m.account_id
             WHERE x.id = ?1",
        )?
        .query_row([attachment.0], |r| Ok((AccountId(r.get(0)?), r.get(1)?)))
        .optional()?)
}

/// `UiCommand::DeleteAttachment`: one transaction removes the attachment (row, files, its
/// place in the stored bodies; a message left with nothing becomes a delete-for-me tombstone)
/// and stores the durable command that tells our other devices, with the names the
/// attachment had. The rule is [`actions::delete_attachment`], the one behind the row's flag.
///
/// No command is stored for an account that is not linked, on a network that does not sync
/// deletes for me, or for an attachment nothing names (a message stored before its backend
/// gave names): the delete is local then.
pub(crate) fn delete_attachment(
    conn: &mut Connection,
    scope: MsgIdScope,
    attachment: AttachmentId,
    now: i64,
) -> Result<LocalDelete> {
    let gone = || CoreError::refused(Refusal::Gone, "unknown attachment");
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let found: Option<(i64, i64, i64, bool)> = tx
        .prepare_cached(
            "SELECT x.message_id, x.idx, m.account_id, x.long_text FROM attachments x
             JOIN messages m ON m.id = x.message_id
             WHERE x.id = ?1 AND m.expiry_hidden = 0",
        )?
        .query_row([attachment.0], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })
        .optional()?;
    let (message, idx, account, long_text) = found.ok_or_else(gone)?;
    let account = AccountId(account);
    let mut apply = Apply::begin(&tx, account, scope, now)?;
    let row = apply.msg_row(MessageId(message))?.ok_or_else(gone)?;
    let (_, key, features) = conversation(&tx, row.conv)?.ok_or_else(gone)?;
    let body = apply.body_of(row.id)?;
    let sending = if row.is_own() {
        actions::sending_of(&tx, row.id, false)?
    } else {
        Sending::Settled
    };
    let queued = row.is_own() && actions::send_waits(&tx, row.id)?;
    actions::delete_attachment(&Facts {
        active: apply.acc.state == AccountState::Active,
        registered: true,
        blocked: false,
        features: features.as_ref(),
        admin: false,
        own: row.is_own(),
        body_kind: &row.body_kind,
        deleted: row.deleted.as_deref(),
        sent_at: row.sent_at,
        revision: row.revision,
        media: actions::media_of(&body, features.as_ref()),
        has_text: actions::has_text(&body),
        sending,
        note_to_self: false,
        queued,
    })?;
    if long_text {
        // The rest of the message's text, not a file of it.
        return Err(CoreError::refused(
            Refusal::WrongKind,
            "this message has no attachment",
        ));
    }
    let at = usize::try_from(idx).map_err(|_| gone())?;
    let net_ids = match &body {
        Body::Media { attachments, .. } => attachments.get(at).map(|a| a.net_ids.clone()),
        Body::Sticker(a) if at == 0 => Some(a.net_ids.clone()),
        _ => None,
    }
    .ok_or_else(gone)?;
    let author = (!row.net_author.is_empty())
        .then(|| sql::user(&row.net_author))
        .transpose()?;
    let target = MsgTarget::to_id(
        Some(key.clone()),
        &NetMsgId {
            author,
            local: row.net_local.as_str().into(),
        },
    );
    apply.remove_attachment(&row, body, at)?;
    let mut commands = Vec::new();
    if apply.syncs_deletes(features.as_ref()) && !net_ids.is_empty() {
        commands.push(apply.queue_command(DurableCommand::DeleteAttachment {
            conv: key,
            target,
            index: u16::try_from(at).unwrap_or(u16::MAX),
            net_ids: net_ids.into(),
        })?);
    }
    apply.finish()?;
    let changes = std::mem::take(&mut apply.ch);
    drop(apply);
    tx.commit()?;
    Ok(LocalDelete {
        account,
        changes,
        commands,
    })
}

#[cfg(test)]
mod tests {
    use kourier_model::{MsgIdScope, NetworkKind, RecipientState, UserId};

    /// The plan of `sql` with its parameters `?1..` replaced by `values`.
    fn plan(h: &crate::testing::IngestHarness, sql: &str, values: &[&str]) -> Vec<String> {
        let mut text = sql.to_owned();
        for (i, v) in values.iter().enumerate().rev() {
            text = text.replace(&format!("?{}", i + 1), v);
        }
        h.connection()
            .prepare(&format!("EXPLAIN QUERY PLAN {text}"))
            .expect("prepare")
            .query_map([], |r| r.get::<_, String>(3))
            .expect("plan")
            .collect::<kourier_db::rusqlite::Result<_>>()
            .expect("rows")
    }

    /// T23: the lookup of the cut that every history batch makes, and the statements that write
    /// it and the keys of what it refuses, are key lookups (a batch of 500 rows must not walk the
    /// account's cuts or its expired keys, and `core.db` has no planner statistics). F8: so are
    /// the clear by time and the clean-up after a merge. They walk the CONVERSATION's rows (the
    /// time has no index of its own: `messages_timeline` by conversation), never the account's
    /// messages, and a clear that is delivered again does not run the first one at all (F2).
    #[test]
    fn the_cut_of_a_history_row_is_one_key_lookup() {
        let dir = tempfile::tempdir().expect("tempdir");
        let h = crate::testing::IngestHarness::open(
            &dir.path().join("core.db"),
            &kourier_db::DbKey::from([4u8; 32]),
            NetworkKind::Fake,
            MsgIdScope::ConversationAuthor,
            &[UserId::parse("aci:me").expect("user id")],
        )
        .expect("harness");
        let cut_args = ["1", "'d:pn:1'", "5", "5"];
        assert_eq!(
            plan(&h, super::CUT_OF, &cut_args),
            ["SEARCH conversation_cuts USING PRIMARY KEY (account_id=? AND conv_key=?)"],
            "{}",
            super::CUT_OF
        );
        for (name, sql, values) in [
            ("the cut", super::STORE_CUT, &cut_args[..]),
            ("the user's own cut", super::STORE_OWN_CUT, &cut_args[..]),
            (
                "the key of a refused row",
                super::REMEMBER_REFUSED,
                &["1", "2", "'d:pn:1'", "'lid:7'", "'X1'", "5", "0"][..],
            ),
        ] {
            let steps = plan(&h, sql, values);
            assert!(
                steps
                    .iter()
                    .all(|s| !s.starts_with("SCAN") || s == "SCAN CONSTANT ROW"),
                "{name}: {sql}\n{steps:#?}"
            );
        }
        // The key of a refused row is looked for by its local id, like `expiry::remembered`.
        let steps = plan(
            &h,
            super::REMEMBER_REFUSED,
            &["1", "2", "'d:pn:1'", "'lid:7'", "'X1'", "5", "0"],
        );
        assert!(
            steps
                .iter()
                .any(|s| s.contains("expired_messages") && s.contains("net_local=?")),
            "{steps:#?}"
        );
        // F8: the clear by time and the history clean-up of a merge. A SEARCH of the
        // conversation's rows through the timeline key, whatever the range adds, and the
        // correlated look-ups of `UNSENT` / `DELETE_QUEUED` through the outbox's own index.
        for (name, sql) in [
            ("gone, by time", super::select_gone(super::BY_TIME)),
            ("hidden, by time", super::select_hidden(super::BY_TIME)),
            (
                "gone, history by time",
                super::select_gone(super::HISTORY_BY_TIME),
            ),
        ] {
            let steps = plan(&h, &sql, &["1", "5"]);
            assert!(
                steps
                    .iter()
                    .any(|s| s
                        .starts_with("SEARCH m USING INDEX messages_timeline (conversation_id=?")),
                "{name}: the conversation's rows through `messages_timeline`\n{steps:#?}"
            );
            assert!(
                steps.iter().all(|s| !s.starts_with("SCAN")
                    && !s.contains("LIST SUBQUERY")
                    && !s.contains("messages_key_account")
                    && !s.contains("USING INDEX messages_local")),
                "{name}: a walk of the account's messages or a scan\n{steps:#?}"
            );
        }
    }

    /// [`super::UNSENT`] reads `send_state.state` with a pattern: the JSON of a recipient
    /// that has the message starts as the pattern says, and no other state does.
    #[test]
    fn sent_is_matched_by_its_json() {
        let json = |s: RecipientState| crate::sql::to_json(&s).expect("json");
        for sealed in [false, true] {
            assert!(json(RecipientState::Sent { sealed }).starts_with("{\"Sent\""));
        }
        assert!(!json(RecipientState::Pending).starts_with("{\"Sent\""));
        for retryable in [false, true] {
            assert!(!json(RecipientState::Failed { retryable }).starts_with("{\"Sent\""));
        }
        assert!(super::UNSENT.contains("LIKE '{\"Sent\"%'"));
    }
}
