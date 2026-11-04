//! What Kourier keeps about TDLib's chats and messages (`backend.db` schema 2, TG-3 refresh §4):
//! the per-chat cursor, the recently delivered messages, the gaps, the parked facts and whether
//! the last helper run ended cleanly.
//!
//! TDLib has no ack: a message it streamed is gone from its stream once its pts is saved. So every
//! fact about delivery moves in ONE transaction with the `core_outbox` rows that carry the
//! messages ([`commit_rows`]): the cursor (`chat_state.delivered_max_id`, never moved down), the
//! `delivered` rows, the gaps opened and closed, the parked deletes, the hash gates and the clean
//! end. A crash leaves either all of it or none (kill points [`INSIDE_ROWS`] and [`AFTER_ROWS`]).
//!
//! The rules:
//! - One `core_outbox` row per [`Row`] (a message with its conversation's and author's upserts
//!   before it, WhatsApp's "one inbox row = one core_outbox row"); a gated event whose hash equals
//!   the one stated last is left out, and a row left empty by its gates is not written.
//! - A delivered message moves its chat's cursor up (`max`), `low_id` down, and `updated_ms` only
//!   when the cursor moved.
//! - A gap row (`gap`) is opened, paged, closed or marked lost only through [`commit_rows`], so a
//!   gap closes only with the rows that fill it (critic B1).
//! - Deletes are parked in `deferred` as they came, ids never delivered included (decision G-8, his
//!   "Keep them; KT-2 first"); a delete never creates a row.
//! - [`sweep`] keeps every `delivered` row that is newer than 14 days, among a chat's newest 200, or
//!   an incoming row above the chat's read position (critic M4: the unread hint counts them).
//!
//! Nothing here logs a value: errors name the operation (`StoreError`), the SQLite text is never
//! kept. `Debug` of the staged items prints ids, kinds and counts only (an event is printed as its
//! type name).

use std::fmt;

use kourier_backend_api::{BackendEvent, encode_outbox_row};
use kourier_db::rusqlite::{Connection, OptionalExtension, params};
use kourier_db::{failpoint, outbox};
use sha2::{Digest, Sha256};

use crate::error::{StoreError, sql};

/// The kill point after every write of a [`commit_rows`], before its commit.
pub(crate) const INSIDE_ROWS: &str = "tg-inside-rows";
/// The kill point right after the commit of a [`commit_rows`].
pub(crate) const AFTER_ROWS: &str = "tg-after-rows";
/// The kill point after a clean stop's `run_state.clean_end = 1` was written, before its commit.
pub(crate) const INSIDE_CLEAN_END: &str = "tg-inside-clean-end";

/// How long a `delivered` row is kept at least (WhatsApp's ledger retention; lead L-5).
pub(crate) const DELIVERED_KEEP_MS: i64 = 14 * 24 * 60 * 60 * 1000;
/// How many of a chat's newest `delivered` rows are kept whatever their age (lead L-5).
pub(crate) const DELIVERED_KEEP_PER_CHAT: i64 = 200;
/// The version of the mapping a parked fact was written under: KT-2 raises it and maps the rows
/// of an older version at the next start.
pub(crate) const MAP_VERSION: i64 = 1;

/// What kind of chat a `chat_state` row is (`chat_state.kind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChatKind {
    /// A private chat (also a bot's chat).
    Direct,
    /// A basic group.
    Group,
    /// A supergroup (its own message box: per-chat ids).
    Supergroup,
    /// A broadcast channel.
    Channel,
    /// A supergroup with topics.
    Forum,
    /// Not known yet (a fact about a chat came before its `updateNewChat`).
    Unknown,
}

impl ChatKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Group => "group",
            Self::Supergroup => "supergroup",
            Self::Channel => "channel",
            Self::Forum => "forum",
            Self::Unknown => "unknown",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "direct" => Self::Direct,
            "group" => Self::Group,
            "supergroup" => Self::Supergroup,
            "channel" => Self::Channel,
            "forum" => Self::Forum,
            "unknown" => Self::Unknown,
            _ => return None,
        })
    }
}

/// Why a chat is held (not mapped) until a later step (decision H5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Held {
    /// A broadcast channel (KT-4, T7).
    Channel,
    /// A bot's chat (sponsored messages, T7).
    Bot,
    /// A public supergroup (sponsored messages, T7).
    Public,
    /// A channel's direct-messages group (T7).
    DirectMessages,
    /// A user or a supergroup TDLib marks restricted (`restriction_reason`, TF13): held like the
    /// others until T7 decides what Kourier may show of it.
    Restricted,
}

impl Held {
    fn as_str(self) -> &'static str {
        match self {
            Self::Channel => "channel",
            Self::Bot => "bot",
            Self::Public => "public",
            Self::DirectMessages => "direct-messages",
            Self::Restricted => "restricted",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "channel" => Self::Channel,
            "bot" => Self::Bot,
            "public" => Self::Public,
            "direct-messages" => Self::DirectMessages,
            "restricted" => Self::Restricted,
            _ => return None,
        })
    }
}

/// Why a gap was opened (`gap.cause`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GapCause {
    /// A supergroup message whose server id jumped past the cursor (decision G-8).
    IdJump,
    /// The first live message of a chat after an unclean end (critic B1).
    UncleanEnd,
    /// The start's reconcile found TDLib's last message above the cursor.
    Start,
    /// A reconcile's snapshot range, written before its first fetch (critic B1).
    Snapshot,
}

impl GapCause {
    fn as_str(self) -> &'static str {
        match self {
            Self::IdJump => "id-jump",
            Self::UncleanEnd => "unclean-end",
            Self::Start => "start",
            Self::Snapshot => "snapshot",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "id-jump" => Self::IdJump,
            "unclean-end" => Self::UncleanEnd,
            "start" => Self::Start,
            "snapshot" => Self::Snapshot,
            _ => return None,
        })
    }
}

/// Whose hash gates an event: a conversation patch of a chat, a contact patch of a user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Gate {
    /// `chat_state.stated_hash` of this chat.
    Chat(i64),
    /// `user_state.stated_hash` of this user.
    User(i64),
    /// Written WHATEVER the chat's stated hash, which it then records: a chat's patch with its
    /// first message row of a helper run (critic L6: the core may have lost the conversation, a
    /// deleted chat comes back with its title).
    Restate(i64),
}

/// One event of a row, with the gate that may leave it out.
#[derive(Clone)]
pub(crate) struct Event {
    /// The event.
    pub(crate) event: BackendEvent,
    /// `Some`: written only when its hash differs from the one stated last for that chat or user.
    pub(crate) gate: Option<Gate>,
}

impl fmt::Debug for Event {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Event")
            .field("event", &self.event.name())
            .field("gate", &self.gate)
            .finish()
    }
}

/// A message handed to the core: what `delivered` keeps of it.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Delivered {
    /// TDLib's chat id.
    pub(crate) chat_id: i64,
    /// TDLib's full message id.
    pub(crate) msg_id: i64,
    /// The author as the core knows it (`tg:<id>`).
    pub(crate) author: String,
    /// Sent by the own account.
    pub(crate) outgoing: bool,
}

impl fmt::Debug for Delivered {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Delivered")
            .field("chat_id", &self.chat_id)
            .field("msg_id", &self.msg_id)
            .field("outgoing", &self.outgoing)
            .finish_non_exhaustive()
    }
}

/// One `core_outbox` row and the messages it delivers.
#[derive(Debug, Clone)]
pub(crate) struct Row {
    /// The events, in order (upserts before the message).
    pub(crate) events: Vec<Event>,
    /// The messages this row carries (one for a live message, many for a history batch).
    pub(crate) delivered: Vec<Delivered>,
}

/// What a commit states about a chat; `None` fields stay as they are. Read positions and
/// `verified_max_id` only move up. `Debug` is hand-written: a photo's remote unique id is a handle
/// that names the file for this account, so it prints as set or cleared only (TF16).
#[derive(Clone, Default, PartialEq, Eq)]
pub(crate) struct ChatFacts {
    /// TDLib's chat id.
    pub(crate) chat_id: i64,
    /// The chat's kind.
    pub(crate) kind: Option<ChatKind>,
    /// Has a position in the main or the archive list, or a delivered message.
    pub(crate) listed: Option<bool>,
    /// `Some(None)`: mapped; `Some(Some(h))`: held for `h`.
    pub(crate) held: Option<Option<Held>>,
    /// Continuity known up to here (critic B1).
    pub(crate) verified_max_id: Option<i64>,
    /// `last_read_inbox_message_id`.
    pub(crate) read_inbox_max_id: Option<i64>,
    /// `last_read_outbox_message_id`.
    pub(crate) read_outbox_max_id: Option<i64>,
    /// The small photo's remote unique id (`Some(None)` clears it).
    pub(crate) photo_small: Option<Option<String>>,
    /// The big photo's remote unique id (`Some(None)` clears it).
    pub(crate) photo_big: Option<Option<String>>,
    /// A basic group's supergroup.
    pub(crate) moved_to: Option<i64>,
}

impl fmt::Debug for ChatFacts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let photo = |p: &Option<Option<String>>| match p {
            None => "unchanged",
            Some(None) => "cleared",
            Some(Some(_)) => "set",
        };
        f.debug_struct("ChatFacts")
            .field("chat_id", &self.chat_id)
            .field("kind", &self.kind)
            .field("listed", &self.listed)
            .field("held", &self.held)
            .field("verified_max_id", &self.verified_max_id)
            .field("read_inbox_max_id", &self.read_inbox_max_id)
            .field("read_outbox_max_id", &self.read_outbox_max_id)
            .field("photo_small", &photo(&self.photo_small))
            .field("photo_big", &photo(&self.photo_big))
            .field("moved_to", &self.moved_to)
            .finish()
    }
}

/// A change of a gap row; `above` is the gap's key with its chat.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GapChange {
    /// Opens `(above, below)`; an open gap with the same `above` is widened to the larger `below`.
    Open {
        /// TDLib's chat id.
        chat_id: i64,
        /// The newest id known to be delivered below the gap (exclusive).
        above: i64,
        /// The id the gap ends at (exclusive): the first message seen after it.
        below: i64,
        /// Why.
        cause: GapCause,
    },
    /// One reconcile page was read: the gap now ends at `below` (the oldest id fetched).
    Paged {
        /// TDLib's chat id.
        chat_id: i64,
        /// The gap's key.
        above: i64,
        /// The new end.
        below: i64,
    },
    /// The gap is filled: its rows are in the same commit.
    Close {
        /// TDLib's chat id.
        chat_id: i64,
        /// The gap's key.
        above: i64,
    },
    /// The reconcile's cap was reached: kept for KT-3, no notice (decision G-5).
    Lost {
        /// TDLib's chat id.
        chat_id: i64,
        /// The gap's key.
        above: i64,
    },
}

/// A fact parked for a later step.
#[derive(Clone, PartialEq, Eq)]
pub(crate) enum Parked {
    /// A permanent delete (`updateDeleteMessages{is_permanent, !from_cache}`): every id as TDLib
    /// named it, delivered or not.
    Delete {
        /// TDLib's chat id.
        chat_id: i64,
        /// TDLib's full message ids.
        ids: Vec<i64>,
    },
}

impl fmt::Debug for Parked {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Delete { chat_id, ids } => f
                .debug_struct("Delete")
                .field("chat_id", chat_id)
                .field("ids", &ids.len())
                .finish(),
        }
    }
}

/// Everything one group commit writes.
#[derive(Debug, Clone, Default)]
pub(crate) struct Staged {
    /// Chat facts, applied first.
    pub(crate) chats: Vec<ChatFacts>,
    /// The rows, in order.
    pub(crate) rows: Vec<Row>,
    /// Gap changes, in order.
    pub(crate) gaps: Vec<GapChange>,
    /// Parked facts.
    pub(crate) parked: Vec<Parked>,
    /// The last commit of a clean stop: `run_state.clean_end = 1`.
    pub(crate) clean_end: bool,
}

impl Staged {
    /// Nothing to write.
    pub(crate) fn is_empty(&self) -> bool {
        self.chats.is_empty()
            && self.rows.is_empty()
            && self.gaps.is_empty()
            && self.parked.is_empty()
            && !self.clean_end
    }
}

/// What one [`commit_rows`] wrote.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Committed {
    /// The `seq` of each `core_outbox` row written, in order.
    pub(crate) seqs: Vec<i64>,
    /// Events left out by their gate.
    pub(crate) gated: usize,
}

/// Writes `staged` in ONE transaction: chat facts, the cursor and `delivered`, the gaps, the
/// parked facts, the rows (gated) and the clean end. Refused as a whole (nothing written) when any
/// part fails.
pub(crate) fn commit_rows(
    conn: &Connection,
    staged: &Staged,
    now_ms: i64,
) -> Result<Committed, StoreError> {
    let tx = conn.unchecked_transaction().map_err(sql("begin rows"))?;
    for facts in &staged.chats {
        chat_facts(&tx, facts, now_ms)?;
    }
    cursor(&tx, staged, now_ms)?;
    gaps(&tx, &staged.gaps, now_ms)?;
    for parked in &staged.parked {
        park(&tx, parked, now_ms)?;
    }
    let mut committed = Committed::default();
    for row in &staged.rows {
        let mut events = Vec::with_capacity(row.events.len());
        for ev in &row.events {
            if gate_passes(&tx, ev, now_ms)? {
                events.push(ev.event.clone());
            } else {
                committed.gated += 1;
            }
        }
        if events.is_empty() {
            continue;
        }
        let bytes = encode_outbox_row(&events).map_err(|_| StoreError("encode rows"))?;
        let seq = outbox::append(&tx, &bytes, now_ms).map_err(|e| {
            tracing::error!(error = %e, "appending Telegram rows failed");
            StoreError("append rows")
        })?;
        committed.seqs.push(seq);
    }
    if staged.clean_end {
        tx.execute(
            "INSERT INTO run_state(id, clean_end, ended_ms) VALUES (1, 1, ?1)
             ON CONFLICT(id) DO UPDATE SET clean_end = 1, ended_ms = ?1",
            params![now_ms],
        )
        .map_err(sql("clean end"))?;
        failpoint::hit(INSIDE_CLEAN_END);
    }
    failpoint::hit(INSIDE_ROWS);
    tx.commit().map_err(sql("commit rows"))?;
    failpoint::hit(AFTER_ROWS);
    Ok(committed)
}

/// Creates the chat's row when it has none (kind `unknown` until a fact says more).
fn ensure_chat(conn: &Connection, chat_id: i64, now_ms: i64) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO chat_state(chat_id, kind, updated_ms) VALUES (?1, 'unknown', ?2)
         ON CONFLICT(chat_id) DO NOTHING",
        params![chat_id, now_ms],
    )
    .map_err(sql("create chat state"))?;
    Ok(())
}

fn chat_facts(conn: &Connection, f: &ChatFacts, now_ms: i64) -> Result<(), StoreError> {
    ensure_chat(conn, f.chat_id, now_ms)?;
    let held_set = f.held.is_some();
    let held = f.held.flatten().map(Held::as_str);
    let small_set = f.photo_small.is_some();
    let big_set = f.photo_big.is_some();
    conn.execute(
        "UPDATE chat_state SET
           kind = coalesce(?2, kind),
           listed = coalesce(?3, listed),
           held = CASE WHEN ?4 THEN ?5 ELSE held END,
           verified_max_id = max(verified_max_id, coalesce(?6, 0)),
           read_inbox_max_id = max(read_inbox_max_id, coalesce(?7, 0)),
           read_outbox_max_id = max(read_outbox_max_id, coalesce(?8, 0)),
           photo_small = CASE WHEN ?9 THEN ?10 ELSE photo_small END,
           photo_big = CASE WHEN ?11 THEN ?12 ELSE photo_big END,
           moved_to = coalesce(?13, moved_to)
         WHERE chat_id = ?1",
        params![
            f.chat_id,
            f.kind.map(ChatKind::as_str),
            f.listed,
            held_set,
            held,
            f.verified_max_id,
            f.read_inbox_max_id,
            f.read_outbox_max_id,
            small_set,
            f.photo_small.clone().flatten(),
            big_set,
            f.photo_big.clone().flatten(),
            f.moved_to,
        ],
    )
    .map_err(sql("chat facts"))?;
    Ok(())
}

/// The `delivered` rows and the cursor moves of every row of the commit.
fn cursor(conn: &Connection, staged: &Staged, now_ms: i64) -> Result<(), StoreError> {
    for d in staged.rows.iter().flat_map(|r| &r.delivered) {
        ensure_chat(conn, d.chat_id, now_ms)?;
        conn.execute(
            "INSERT INTO delivered(chat_id, msg_id, author, outgoing, at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT(chat_id, msg_id) DO NOTHING",
            params![d.chat_id, d.msg_id, d.author, d.outgoing, now_ms],
        )
        .map_err(sql("delivered"))?;
        conn.execute(
            "UPDATE chat_state SET
               updated_ms = CASE WHEN ?2 > delivered_max_id THEN ?3 ELSE updated_ms END,
               delivered_max_id = max(delivered_max_id, ?2),
               low_id = CASE WHEN low_id = 0 OR ?2 < low_id THEN ?2 ELSE low_id END
             WHERE chat_id = ?1",
            params![d.chat_id, d.msg_id, now_ms],
        )
        .map_err(sql("move cursor"))?;
    }
    Ok(())
}

fn gaps(conn: &Connection, changes: &[GapChange], now_ms: i64) -> Result<(), StoreError> {
    for change in changes {
        match *change {
            GapChange::Open {
                chat_id,
                above,
                below,
                cause,
            } => {
                conn.execute(
                    "INSERT INTO gap(chat_id, above, below, cause, state, pages, opened_ms)
                     VALUES (?1, ?2, ?3, ?4, 'open', 0, ?5)
                     ON CONFLICT(chat_id, above) DO UPDATE SET below = max(below, excluded.below)
                       WHERE gap.state = 'open'",
                    params![chat_id, above, below, cause.as_str(), now_ms],
                )
                .map_err(sql("open gap"))?;
            }
            GapChange::Paged {
                chat_id,
                above,
                below,
            } => {
                conn.execute(
                    "UPDATE gap SET below = ?3, pages = pages + 1 WHERE chat_id = ?1 AND above = ?2",
                    params![chat_id, above, below],
                )
                .map_err(sql("page gap"))?;
            }
            GapChange::Close { chat_id, above } => {
                conn.execute(
                    "DELETE FROM gap WHERE chat_id = ?1 AND above = ?2",
                    params![chat_id, above],
                )
                .map_err(sql("close gap"))?;
            }
            GapChange::Lost { chat_id, above } => {
                conn.execute(
                    "UPDATE gap SET state = 'lost' WHERE chat_id = ?1 AND above = ?2",
                    params![chat_id, above],
                )
                .map_err(sql("lose gap"))?;
            }
        }
    }
    Ok(())
}

fn park(conn: &Connection, parked: &Parked, now_ms: i64) -> Result<(), StoreError> {
    match parked {
        Parked::Delete { chat_id, ids } => {
            let payload = serde_json::to_string(&serde_json::json!({ "ids": ids }))
                .map_err(|_| StoreError("encode parked delete"))?;
            conn.execute(
                "INSERT INTO deferred(chat_id, kind, payload, map_version, at_ms)
                 SELECT ?1, 'delete', ?2, ?3, ?4",
                params![chat_id, payload, MAP_VERSION, now_ms],
            )
            .map_err(sql("park delete"))?;
        }
    }
    Ok(())
}

/// The gate's hash of an event: SHA-256 of its JSON (serde writes struct fields in order).
fn event_hash(event: &BackendEvent) -> Result<Vec<u8>, StoreError> {
    let json = serde_json::to_vec(event).map_err(|_| StoreError("hash event"))?;
    Ok(Sha256::digest(&json).to_vec())
}

/// Whether a gated event is written; records its hash when it is.
fn gate_passes(conn: &Connection, ev: &Event, now_ms: i64) -> Result<bool, StoreError> {
    let Some(gate) = ev.gate else {
        return Ok(true);
    };
    let hash = event_hash(&ev.event)?;
    match gate {
        Gate::Chat(chat_id) | Gate::Restate(chat_id) => {
            ensure_chat(conn, chat_id, now_ms)?;
            let stated: Option<Vec<u8>> = conn
                .query_row(
                    "SELECT stated_hash FROM chat_state WHERE chat_id = ?1",
                    params![chat_id],
                    |r| r.get(0),
                )
                .map_err(sql("read chat gate"))?;
            if matches!(gate, Gate::Chat(_)) && stated.as_deref() == Some(hash.as_slice()) {
                return Ok(false);
            }
            conn.execute(
                "UPDATE chat_state SET stated_hash = ?2 WHERE chat_id = ?1",
                params![chat_id, hash],
            )
            .map_err(sql("write chat gate"))?;
        }
        Gate::User(user_id) => {
            let stated: Option<Vec<u8>> = conn
                .query_row(
                    "SELECT stated_hash FROM user_state WHERE user_id = ?1",
                    params![user_id],
                    |r| r.get(0),
                )
                .optional()
                .map_err(sql("read user gate"))?;
            if stated.as_deref() == Some(hash.as_slice()) {
                return Ok(false);
            }
            conn.execute(
                "INSERT INTO user_state(user_id, stated_hash, updated_ms) VALUES (?1, ?2, ?3)
                 ON CONFLICT(user_id) DO UPDATE SET stated_hash = ?2, updated_ms = ?3",
                params![user_id, hash, now_ms],
            )
            .map_err(sql("write user gate"))?;
        }
    }
    Ok(true)
}

/// Forgets the chat's stated hash, so its next patch is stated again (critic L6: the core lost the
/// conversation). Tests only until a caller needs it.
#[cfg(test)]
pub(crate) fn reset_chat_gate(conn: &Connection, chat_id: i64) -> Result<(), StoreError> {
    conn.execute(
        "UPDATE chat_state SET stated_hash = NULL WHERE chat_id = ?1",
        params![chat_id],
    )
    .map_err(sql("reset chat gate"))?;
    Ok(())
}

/// A chat's row. `Debug` is hand-written: the photos' remote ids print as set or none (TF16).
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct ChatRow {
    /// TDLib's chat id.
    pub(crate) chat_id: i64,
    /// Its kind.
    pub(crate) kind: ChatKind,
    /// Listed.
    pub(crate) listed: bool,
    /// Why it is held, if it is.
    pub(crate) held: Option<Held>,
    /// THE CURSOR.
    pub(crate) delivered_max_id: i64,
    /// Continuity known up to here.
    pub(crate) verified_max_id: i64,
    /// The oldest delivered id (0: none).
    pub(crate) low_id: i64,
    /// The read inbox position.
    pub(crate) read_inbox_max_id: i64,
    /// The read outbox position.
    pub(crate) read_outbox_max_id: i64,
    /// The small photo's remote unique id.
    pub(crate) photo_small: Option<String>,
    /// The big photo's remote unique id.
    pub(crate) photo_big: Option<String>,
    /// A basic group's supergroup.
    pub(crate) moved_to: Option<i64>,
    /// When the cursor last moved.
    pub(crate) updated_ms: i64,
}

impl fmt::Debug for ChatRow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let photo = |p: &Option<String>| if p.is_some() { "set" } else { "none" };
        f.debug_struct("ChatRow")
            .field("chat_id", &self.chat_id)
            .field("kind", &self.kind)
            .field("listed", &self.listed)
            .field("held", &self.held)
            .field("delivered_max_id", &self.delivered_max_id)
            .field("verified_max_id", &self.verified_max_id)
            .field("low_id", &self.low_id)
            .field("read_inbox_max_id", &self.read_inbox_max_id)
            .field("read_outbox_max_id", &self.read_outbox_max_id)
            .field("photo_small", &photo(&self.photo_small))
            .field("photo_big", &photo(&self.photo_big))
            .field("moved_to", &self.moved_to)
            .field("updated_ms", &self.updated_ms)
            .finish()
    }
}

/// The chat's row, if it has one.
pub(crate) fn chat(conn: &Connection, chat_id: i64) -> Result<Option<ChatRow>, StoreError> {
    let row = conn
        .query_row(
            "SELECT kind, listed, held, delivered_max_id, verified_max_id, low_id,
                    read_inbox_max_id, read_outbox_max_id, photo_small, photo_big, moved_to,
                    updated_ms
             FROM chat_state WHERE chat_id = ?1",
            params![chat_id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, bool>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    [
                        r.get::<_, i64>(3)?,
                        r.get::<_, i64>(4)?,
                        r.get::<_, i64>(5)?,
                        r.get::<_, i64>(6)?,
                        r.get::<_, i64>(7)?,
                    ],
                    r.get::<_, Option<String>>(8)?,
                    r.get::<_, Option<String>>(9)?,
                    r.get::<_, Option<i64>>(10)?,
                    r.get::<_, i64>(11)?,
                ))
            },
        )
        .optional()
        .map_err(sql("read chat state"))?;
    let Some((kind, listed, held, ids, photo_small, photo_big, moved_to, updated_ms)) = row else {
        return Ok(None);
    };
    let kind = ChatKind::parse(&kind).ok_or(StoreError("chat kind unreadable"))?;
    let held = match held {
        None => None,
        Some(h) => Some(Held::parse(&h).ok_or(StoreError("chat hold unreadable"))?),
    };
    let [
        delivered_max_id,
        verified_max_id,
        low_id,
        read_inbox_max_id,
        read_outbox_max_id,
    ] = ids;
    Ok(Some(ChatRow {
        chat_id,
        kind,
        listed,
        held,
        delivered_max_id,
        verified_max_id,
        low_id,
        read_inbox_max_id,
        read_outbox_max_id,
        photo_small,
        photo_big,
        moved_to,
        updated_ms,
    }))
}

/// What a helper run's machines start from (TG-3 C8, C9a): every chat's row, every `delivered`
/// row and every gap, read once at the run's start. `Debug` prints counts only.
#[derive(Clone, Default)]
pub(crate) struct Seed {
    /// Every `chat_state` row.
    pub(crate) chats: Vec<ChatRow>,
    /// Every `delivered` row.
    pub(crate) delivered: Vec<Delivered>,
    /// Every gap row, open and lost (the reconcile pages the open ones, C9a).
    pub(crate) gaps: Vec<GapRow>,
}

impl fmt::Debug for Seed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Seed")
            .field("chats", &self.chats.len())
            .field("delivered", &self.delivered.len())
            .field("gaps", &self.gaps.len())
            .finish()
    }
}

/// The [`Seed`] of a helper run.
pub(crate) fn seed(conn: &Connection) -> Result<Seed, StoreError> {
    let mut stmt = conn
        .prepare("SELECT chat_id FROM chat_state ORDER BY chat_id")
        .map_err(sql("read chat ids"))?;
    let ids = stmt
        .query_map([], |r| r.get::<_, i64>(0))
        .map_err(sql("read chat ids"))?
        .collect::<Result<Vec<i64>, _>>()
        .map_err(sql("read chat ids"))?;
    let mut chats = Vec::with_capacity(ids.len());
    for id in ids {
        if let Some(row) = chat(conn, id)? {
            chats.push(row);
        }
    }
    let mut stmt = conn
        .prepare("SELECT chat_id, msg_id, author, outgoing FROM delivered ORDER BY chat_id, msg_id")
        .map_err(sql("read delivered"))?;
    let delivered = stmt
        .query_map([], |r| {
            Ok(Delivered {
                chat_id: r.get(0)?,
                msg_id: r.get(1)?,
                author: r.get(2)?,
                outgoing: r.get(3)?,
            })
        })
        .map_err(sql("read delivered"))?
        .collect::<Result<Vec<Delivered>, _>>()
        .map_err(sql("read delivered"))?;
    Ok(Seed {
        chats,
        delivered,
        gaps: gap_rows(conn)?,
    })
}

/// A gap row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct GapRow {
    /// TDLib's chat id.
    pub(crate) chat_id: i64,
    /// Above (exclusive).
    pub(crate) above: i64,
    /// Below (exclusive).
    pub(crate) below: i64,
    /// Why it was opened.
    pub(crate) cause: GapCause,
    /// `false`: open; `true`: lost at the cap.
    pub(crate) lost: bool,
    /// Pages the reconcile read for it.
    pub(crate) pages: i64,
}

/// Every gap, by chat and `above`.
pub(crate) fn gap_rows(conn: &Connection) -> Result<Vec<GapRow>, StoreError> {
    let mut stmt = conn
        .prepare(
            "SELECT chat_id, above, below, cause, state, pages FROM gap ORDER BY chat_id, above",
        )
        .map_err(sql("read gaps"))?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, i64>(5)?,
            ))
        })
        .map_err(sql("read gaps"))?;
    let mut out = Vec::new();
    for row in rows {
        let (chat_id, above, below, cause, state, pages) = row.map_err(sql("read gaps"))?;
        out.push(GapRow {
            chat_id,
            above,
            below,
            cause: GapCause::parse(&cause).ok_or(StoreError("gap cause unreadable"))?,
            lost: state == "lost",
            pages,
        });
    }
    Ok(out)
}

/// The author and direction of a delivered message, if `delivered` still holds it (tests only: the
/// machines keep the window in memory).
#[cfg(test)]
pub(crate) fn delivered_author(
    conn: &Connection,
    chat_id: i64,
    msg_id: i64,
) -> Result<Option<(String, bool)>, StoreError> {
    conn.query_row(
        "SELECT author, outgoing FROM delivered WHERE chat_id = ?1 AND msg_id = ?2",
        params![chat_id, msg_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .optional()
    .map_err(sql("read delivered"))
}

/// The incoming delivered messages above the chat's read position (the unread rows the core
/// counts; critic M4). Tests only.
#[cfg(test)]
pub(crate) fn delivered_unread(conn: &Connection, chat_id: i64) -> Result<i64, StoreError> {
    conn.query_row(
        "SELECT count(*) FROM delivered d JOIN chat_state c ON c.chat_id = d.chat_id
         WHERE d.chat_id = ?1 AND d.outgoing = 0 AND d.msg_id > c.read_inbox_max_id",
        params![chat_id],
        |r| r.get(0),
    )
    .map_err(sql("count unread delivered"))
}

/// The newest delivered messages up to `max_id` (inclusive), newest first, at most `limit`, as
/// `(id, outgoing)` (the read act's pages; the act's highest INCOMING id is what its confirm
/// stores, TG-4 K4, critic M4).
pub(crate) fn delivered_ids(
    conn: &Connection,
    chat_id: i64,
    max_id: i64,
    limit: i64,
) -> Result<Vec<(i64, bool)>, StoreError> {
    let mut stmt = conn
        .prepare(
            "SELECT msg_id, outgoing FROM delivered WHERE chat_id = ?1 AND msg_id <= ?2
             ORDER BY msg_id DESC LIMIT ?3",
        )
        .map_err(sql("read delivered ids"))?;
    let ids = stmt
        .query_map(params![chat_id, max_id, limit], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .map_err(sql("read delivered ids"))?
        .collect::<Result<Vec<(i64, bool)>, _>>()
        .map_err(sql("read delivered ids"))?;
    Ok(ids)
}

/// A parked fact as stored (tests only: KT-2 reads the table).
#[cfg(test)]
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct ParkedRow {
    /// TDLib's chat id.
    pub(crate) chat_id: i64,
    /// The kind (`delete`).
    pub(crate) kind: String,
    /// The JSON payload.
    pub(crate) payload: String,
    /// The mapping version it was written under.
    pub(crate) map_version: i64,
}

#[cfg(test)]
impl fmt::Debug for ParkedRow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ParkedRow")
            .field("chat_id", &self.chat_id)
            .field("kind", &self.kind)
            .field("map_version", &self.map_version)
            .finish_non_exhaustive()
    }
}

/// Every parked fact, oldest first.
#[cfg(test)]
pub(crate) fn parked_rows(conn: &Connection) -> Result<Vec<ParkedRow>, StoreError> {
    let mut stmt = conn
        .prepare("SELECT chat_id, kind, payload, map_version FROM deferred ORDER BY seq")
        .map_err(sql("read deferred"))?;
    let rows = stmt
        .query_map([], |r| {
            Ok(ParkedRow {
                chat_id: r.get(0)?,
                kind: r.get(1)?,
                payload: r.get(2)?,
                map_version: r.get(3)?,
            })
        })
        .map_err(sql("read deferred"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql("read deferred"))?;
    Ok(rows)
}

/// How the previous helper run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PreviousRun {
    /// No run was recorded (a new file).
    None,
    /// It ended cleanly at `ended_ms`.
    Clean {
        /// When.
        ended_ms: Option<i64>,
    },
    /// It did not end cleanly (a crash, a kill, a power loss).
    Unclean,
}

/// Starts a helper run: reads how the last one ended and records this one as not ended
/// (`clean_end = 0`) until a clean stop's last commit says otherwise.
pub(crate) fn start_run(conn: &Connection) -> Result<PreviousRun, StoreError> {
    let tx = conn.unchecked_transaction().map_err(sql("begin run"))?;
    let row: Option<(bool, Option<i64>)> = tx
        .query_row(
            "SELECT clean_end, ended_ms FROM run_state WHERE id = 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(sql("read run state"))?;
    tx.execute(
        "INSERT INTO run_state(id, clean_end, ended_ms) VALUES (1, 0, NULL)
         ON CONFLICT(id) DO UPDATE SET clean_end = 0",
        [],
    )
    .map_err(sql("start run"))?;
    tx.commit().map_err(sql("commit run"))?;
    Ok(match row {
        None => PreviousRun::None,
        Some((true, ended_ms)) => PreviousRun::Clean { ended_ms },
        Some((false, _)) => PreviousRun::Unclean,
    })
}

/// Deletes the `delivered` rows no rule keeps (module docs); returns how many. Run at a quiet
/// point (after the start sync).
pub(crate) fn sweep(conn: &Connection, now_ms: i64) -> Result<usize, StoreError> {
    conn.execute(
        "DELETE FROM delivered WHERE (chat_id, msg_id) IN (
           SELECT d.chat_id, d.msg_id FROM (
             SELECT chat_id, msg_id, outgoing, at_ms,
                    row_number() OVER (PARTITION BY chat_id ORDER BY msg_id DESC) AS rank
             FROM delivered) d
           LEFT JOIN chat_state c ON c.chat_id = d.chat_id
           WHERE d.at_ms < ?1
             AND d.rank > ?2
             AND (d.outgoing = 1 OR d.msg_id <= coalesce(c.read_inbox_max_id, 0)))",
        params![now_ms - DELIVERED_KEEP_MS, DELIVERED_KEEP_PER_CHAT],
    )
    .map_err(sql("sweep delivered"))
}

#[cfg(test)]
mod tests;
