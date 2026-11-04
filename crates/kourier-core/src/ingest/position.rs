//! Read positions (KT-2, architecture §4.9): a network whose server orders messages states where
//! each one stands (`IncomingMessage::net_seq`) and how far a chat was read, by us on another session
//! (`BackendEvent::ReadUpTo`) and by the other side (`BackendEvent::PeerReadUpTo`).
//!
//! THE RULE: a row with a position is read by position and never by time; a row without one by time
//! and never by position. Telegram's dates are whole seconds: two messages of one second must not
//! share a read state. Both positions only rise, and a message that arrives, or learns its position,
//! at or below one follows it.
//!
//! A chat the core does not have is not made by a position: the event is dropped (one debug line).
//!
//! THE COST is what moved, never the chat. The own position goes through the unread rows at or below
//! it (`messages_unread_seq`; the update is pinned, `INDEXED BY`: no statistics in core.db), the
//! peer's through `messages_seq` and only for `(old, new]`. On a network that numbers nothing `messages_seq` is empty and these statements do not
//! even run; the one that does, `local::mark_read`'s, is one probe of it (`tests/cloud_ids.rs`).

use kourier_db::rusqlite::OptionalExtension;
use kourier_model::{ConvKey, ConversationId, MessageId, OutgoingState, ReadState};

use super::Apply;
use super::message::{MsgRow, merge_out};
use crate::error::Result;
use crate::sql;

/// SQL: marks every unread message of the conversation `?1` at or below the position `?2` read, and
/// returns their ids. A row with no position is not touched: it follows the time rule
/// ([`super::READ_UP_TO`]). Pinned to the unread rows that have a position, by position: it walks
/// what it marks and nothing else (not the chat's other unread rows, which sit above the position).
pub(crate) const READ_BY_POSITION: &str = "UPDATE messages INDEXED BY messages_unread_seq
   SET read_state = 'Read'
 WHERE conversation_id = ?1 AND read_state = 'Unread' AND net_seq <= ?2
 RETURNING id";

/// SQL: the sort key of the newest row of the conversation `?1` at or below the position `?2`: how
/// far the chat is read (`read_up_to`).
pub(crate) const NEWEST_AT_POSITION: &str = "SELECT sort_key FROM messages
 WHERE conversation_id = ?1 AND net_seq <= ?2 ORDER BY net_seq DESC LIMIT 1";

/// SQL: our own rows of the conversation `?1` in `(?2, ?3]`, with their floors: what a marker of the
/// other side moved.
pub(crate) const PEER_READ_RANGE: &str = "SELECT id, out_floor FROM messages
 WHERE conversation_id = ?1 AND net_seq > ?2 AND net_seq <= ?3 AND net_author = ''";

impl Apply<'_> {
    /// `BackendEvent::ReadUpTo`: our other session read `conv` up to `seq`.
    pub(super) fn read_up_to(&mut self, conv: &ConvKey, seq: i64) -> Result<()> {
        let key = self.canon_conv(conv)?;
        let Some(conv) = sql::conversation_id(self.conn, self.acc.id, &key)? else {
            tracing::debug!(
                account = self.acc.id.0,
                "a read position of a chat the core does not have was dropped"
            );
            return Ok(());
        };
        self.read_at_position(conv, seq)
    }

    /// The own read position of `conv` rises to `seq`: `read_up_to` follows and every unread row at
    /// or below it goes read. A position that does not rise moves the position nowhere, and still
    /// reads what is unread at or below it: a row a read here passed by (one the sweeper hid until
    /// its clock is confirmed, shown again) is read when the other session says so. That costs the
    /// unread rows at or below the position (`messages_unread_seq`), so a replay, or a server that
    /// restates its position with every new message, walks none.
    pub(super) fn read_at_position(&mut self, conv: ConversationId, seq: i64) -> Result<()> {
        let rose = self
            .conn
            .prepare_cached(
                "UPDATE conversations SET read_seq = ?2
                 WHERE id = ?1 AND (read_seq IS NULL OR read_seq < ?2)",
            )?
            .execute((conv.0, seq))?;
        if rose > 0 {
            let newest: Option<i64> = self
                .conn
                .prepare_cached(NEWEST_AT_POSITION)?
                .query_row((conv.0, seq), |r| r.get(0))
                .optional()?;
            if let Some(sort_key) = newest {
                self.conn
                    .prepare_cached(
                        "UPDATE conversations SET read_up_to = max(coalesce(read_up_to, ?2), ?2)
                         WHERE id = ?1",
                    )?
                    .execute((conv.0, sort_key))?;
            }
        }
        let read = self.read_rows_at(conv, seq)?;
        if rose > 0 || read > 0 {
            self.ch.conversations.insert(conv);
            self.ch.recount_conv(conv);
        }
        Ok(())
    }

    /// Every unread row of `conv` at or below `seq` goes read, its `AfterRead` timer starts. How many
    /// did.
    pub(super) fn read_rows_at(&mut self, conv: ConversationId, seq: i64) -> Result<usize> {
        let ids: Vec<i64> = self
            .conn
            .prepare_cached(READ_BY_POSITION)?
            .query_map((conv.0, seq), |r| r.get(0))?
            .collect::<kourier_db::rusqlite::Result<_>>()?;
        for &id in &ids {
            self.start_read_timer(MessageId(id))?;
            self.ch.message(conv, MessageId(id));
        }
        Ok(ids.len())
    }

    /// `BackendEvent::PeerReadUpTo`: the other side read our messages of `conv` up to `seq`.
    pub(super) fn peer_read_up_to(&mut self, conv: &ConvKey, seq: i64) -> Result<()> {
        let key = self.canon_conv(conv)?;
        let Some(conv) = sql::conversation_id(self.conn, self.acc.id, &key)? else {
            tracing::debug!(
                account = self.acc.id.0,
                "a position the other side read of a chat the core does not have was dropped"
            );
            return Ok(());
        };
        let old: Option<i64> = self
            .conn
            .prepare_cached("SELECT peer_read_seq FROM conversations WHERE id = ?1")?
            .query_row([conv.0], |r| r.get(0))?;
        if old.is_some_and(|old| seq <= old) {
            return Ok(());
        }
        self.conn
            .prepare_cached("UPDATE conversations SET peer_read_seq = ?2 WHERE id = ?1")?
            .execute((conv.0, seq))?;
        // (`net_seq > NULL` matches nothing: the first marker starts at the bottom.)
        self.floors_read_between(conv, old.unwrap_or(i64::MIN), seq)
    }

    /// Our own rows of `conv` in `(above, up_to]` are read by the other side.
    pub(super) fn floors_read_between(
        &mut self,
        conv: ConversationId,
        above: i64,
        up_to: i64,
    ) -> Result<()> {
        let rows: Vec<(i64, Option<String>)> = self
            .conn
            .prepare_cached(PEER_READ_RANGE)?
            .query_map((conv.0, above, up_to), |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<kourier_db::rusqlite::Result<_>>()?;
        for (id, floor) in rows {
            self.floor_read(conv, MessageId(id), floor)?;
        }
        Ok(())
    }

    /// The floor of an own message rises to `Read`. A row without a floor (a call line, a notice of
    /// our own doing) has no author either, and was not sent: nothing can read it.
    pub(super) fn floor_read(
        &mut self,
        conv: ConversationId,
        id: MessageId,
        floor: Option<String>,
    ) -> Result<()> {
        let Some(floor) = floor else {
            return Ok(());
        };
        let floor: Option<OutgoingState> = Some(sql::from_json(&floor)?);
        let raised = merge_out(floor.clone(), Some(OutgoingState::Read));
        if raised != floor {
            self.conn
                .prepare_cached("UPDATE messages SET out_floor = ?2 WHERE id = ?1")?
                .execute((id.0, raised.as_ref().map(sql::to_json).transpose()?))?;
            self.ch.message(conv, id);
        }
        self.recompute_out(id)
    }

    /// `row` had no position and a later copy states `seq`: it takes it, and the two position rules
    /// run for that row, once, whatever else the copy changes: a tombstone and a call line take
    /// their place too. Not from a copy of another author under the same id (a collision under
    /// per-conversation ids, `existing`): that copy is not this message. Returns the row as
    /// it stands now.
    pub(super) fn position_learnt(&mut self, row: MsgRow, seq: i64) -> Result<MsgRow> {
        self.conn
            .prepare_cached("UPDATE messages SET net_seq = ?2 WHERE id = ?1")?
            .execute((row.id.0, seq))?;
        let (read, peer, floor): (Option<i64>, Option<i64>, Option<String>) = self
            .conn
            .prepare_cached(
                "SELECT c.read_seq, c.peer_read_seq, m.out_floor FROM messages m
                   JOIN conversations c ON c.id = m.conversation_id
                 WHERE m.id = ?1",
            )?
            .query_row([row.id.0], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        if row.read_state.as_deref() == Some("Unread") && read.is_some_and(|read| seq <= read) {
            self.raise_read_state(&row, ReadState::Read)?;
        }
        if peer.is_some_and(|peer| seq <= peer) {
            self.floor_read(row.conv, row.id, floor)?;
        }
        Ok(self.msg_row(row.id)?.unwrap_or(row))
    }
}
