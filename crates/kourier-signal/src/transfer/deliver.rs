//! The delivery of a committed history import to the core (docs/architecture.md §14.2 item
//! 14; `transfer.rs`, "The import").
//!
//! After the commit zone the stage is whole and authentic. Delivery hands it to the core in
//! small rows of `core_outbox`, with back-pressure and a durable cursor, and a quit or a
//! kill goes on where the cursor stands.
//!
//! # The feeder and the zones
//!
//! A **feeder** on a blocking thread opens the stage read-only and builds typed chunks from
//! where the stored cursor stands ([`feed`]); it hands them over a channel that holds
//! [`DeliveryBounds::window`] of them, so it reads no further than the account's thread
//! takes. The account's thread runs ONE zone per chunk ([`append`]):
//!
//! 1. the look at the cursor: the chunk must begin exactly where the stored cursor stands
//!    (or, for a piece that is appended again, at the stored piece), else the zone fails
//!    and nothing is written;
//! 2. the chunk's backend rows (`peers` and a profile fetch for a person, as the live path
//!    writes them when it first meets one);
//! 3. the chunk's events as ONE `core_outbox` row, after `Envelope::validate`;
//! 4. the row's entry in `history_delivery`;
//! 5. the cursor, moved past the chunk, and the counts.
//!
//! So after a kill at any point every chunk was appended exactly once: the zone either
//! committed (row, entry and cursor together) or did not (none of them), and the next
//! feeder starts from the stored cursor.
//!
//! Order: the people (their `ContactUpsert`), then the chats that are shown, each as a
//! `ConversationUpsert` with its features in front (`features::with_features`), then per
//! chat, the most recent first, its rows in the stage's order as `HistoryBatch`. The
//! upserts go the LEAST recent chat first: the core lists a chat without rows by when it
//! first heard of it, the newest on top, so the list stands in the phone's order before it
//! fills.
//!
//! NO UNREAD COUNT IS SENT (`ConversationPatch::unread_hint` is never set here). The
//! archive says of every message whether the phone had read it, the rows carry that as
//! `IncomingMessage::read`, and the core counts the unread rows (the contract: the hint is
//! only for unread messages a backend has no rows for). A count sent beside the rows came
//! back after the chat was read here while it filled, counted rows that were dropped, and
//! was not lowered when the phone read the chat.
//!
//! # Back-pressure
//!
//! Before the next chunk is taken, fewer than [`DeliveryBounds::window`] delivery rows may
//! be unsettled ([`settle`]): `backend.db` holds two delivery rows at most, of at most 500
//! messages and about 1 MiB each, however large the history is.
//!
//! # Bounds (a delivery never holds receiving back for good)
//!
//! A delivery row is settled when it left `core_outbox`. Its sequence number is then looked
//! up in `core_outbox_poison`: a row the core refused for good is appended again as single
//! messages (it becomes a piece without a row, `history_delivery.seq` NULL, which the next
//! feeder cuts into singles), and a single message the core refuses is dropped and counted
//! (its copy in the poison table is deleted: a refused history must not pile up in
//! `backend.db` with its content). A row that is found unacknowledged at more than
//! [`DeliveryBounds::max_starts`] delivery starts, and the unsettled rows once the delivery
//! has RUN for [`DeliveryBounds::idle`] without any acknowledgement (running time, stored
//! now and then, never the wall clock), are taken back the same way, but ONE row at a time,
//! the oldest: the forwarder ships in order and the core stops at the first job that fails,
//! so the row behind the oldest was never tried (see [`strike`]).
//!
//! Three things end the transfer as `HistoryTransferFailure::Incomplete` (what was delivered
//! stays, receiving starts), each for a core that makes no progress in its own way:
//!
//! - [`DeliveryBounds::max_strikes`] takings back in a row with no answer of the core in
//!   between (a core that answers nothing);
//! - [`DeliveryBounds::max_refused`] single units refused in a row, with no acknowledged row
//!   in between (a core that refuses everything; the bare end of a chat does not count as
//!   an acknowledged row, because a core that refuses every message accepts an empty batch);
//! - [`DeliveryBounds::stuck`]: the total running time all the takings back of the delivery
//!   waited, which nothing resets (a core that cannot store a message here and another
//!   there: each costs two bounds of waiting, and the rows acknowledged around it must not
//!   make the next one free).
//!
//! A dropped unit alone does NOT end the transfer: it ends `done`, with the drops counted
//! (`deliver_dropped`, and in the log).
//!
//! What a zone of a chunk writes into `peers` stays when its row is taken back: a redo of a
//! row of chats forgets that the core has their features first ([`events_of`]), or they
//! would come again without them. A row of chats that is taken back forgets the features of
//! the chats it named at once ([`take_back`], those chats and nobody else's), so that
//! `peers.features_hash`, the backend's record of "the core has this chat", is not stale
//! after a chat's unit is dropped for good or a lost stage discards a pending piece. The
//! pins leave such a chat out, or the core would make a bare chat of it.
//!
//! The 1:1 timer of a chat frame is written into `peers` as a pair with its version, and only
//! when the version is not below the stored one: a chats zone runs again after a redo, and
//! the user may have changed the timer meanwhile.
//!
//! # What a start verifies of the stage
//!
//! It opens the stage with the transfer's key (SQLCipher checks the first page), reads its
//! format and its `complete` mark, and compares its totals and frame count with the
//! recorded ones. It does NOT count the rows or check every page: every page carries a
//! check value under the stage's key and is refused when it is read, so a damaged stage is
//! found at the row that needs the damaged page, before that row is handed on, and a stage
//! that cannot be read to its end is a lost stage (below). At its end the feeder compares
//! the rows it read with the stage's own counters.
//!
//! # A lost stage
//!
//! Nothing of a committed import is ever withdrawn. A stage that is gone or cannot be read
//! is forgotten ([`forget`]): the record is taken back, the cursor set to its start, and
//! the archive is read again, which counts as a start of the read. Message ids are
//! deterministic and the core's ingest is idempotent, so what was delivered before is not
//! doubled. The same happens at a start with a rebuilt `core.db` ([`rebuild`]).
//!
//! Nothing of a row is logged: counts and static reasons only.

use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use kourier_backend_api::{
    BackendEvent, Envelope, IncomingMessage, MAX_HISTORY_BATCH, decode_outbox_row,
};
use kourier_db::rusqlite::{Connection, OptionalExtension as _};
use kourier_db::{Tx, ZoneDb};
use kourier_model::{
    CannotSend, ContactPatch, ConvKey, ConversationFeatures, ConversationPatch, Identifier,
    IdentifierKind, Patch,
};
use libsignal_core::{Aci, Pni, ServiceId};
use tokio::sync::mpsc;
use zeroize::Zeroizing;

use super::stage::{StageError, StageKey, StageReader, StagedChatRow, StagedContact};
use super::{Recorded, to_i64, to_u64};
use crate::actor::Shared;
use crate::error::{Error, Result};
use crate::net::SignalTransport;
use crate::{features, outbox, peers, profile};

/// A kill point where a chunk is about to be appended: nothing of it is written.
pub(crate) const FAILPOINT_BEFORE_CHUNK: &str = "history_delivery_before_chunk";

/// A kill point right after a chunk's zone: its row, its entry and the cursor are durable.
pub(crate) const FAILPOINT_AFTER_CHUNK: &str = "history_delivery_after_chunk";

/// A kill point INSIDE a chunk's zone, after its row and its entry are written and before
/// the cursor is: the transaction is not committed, so a kill here leaves nothing of the
/// chunk (it is hit once per chunk that has a row).
pub(crate) const FAILPOINT_INSIDE_CHUNK: &str = "history_delivery_inside_chunk";

/// A kill point at the commit of a chunk's zone: hit by the commit's own callback, before the
/// zone returns, so it lies between the chunk's row and anything a second zone might write
/// after it (the cursor). A kill here finds the row, its entry AND the cursor durable
/// together (it is hit once per chunk that has a row).
pub(crate) const FAILPOINT_AT_COMMIT: &str = "history_delivery_at_commit";

/// A kill point right after the zone that settles acknowledged rows.
pub(crate) const FAILPOINT_AFTER_SETTLE: &str = "history_delivery_after_settle";

/// A kill point where every row is acknowledged and `done` is not written yet.
pub(crate) const FAILPOINT_BEFORE_DONE: &str = "history_delivery_before_done";

/// The staged bytes one delivery row holds at most (a message larger than this travels
/// alone). The core commits a row in one transaction; a megabyte keeps that transaction and
/// the two rows `backend.db` holds small. Kourier's own bound (architecture §13).
pub(crate) const MAX_ROW_BYTES: u64 = 1024 * 1024;

/// Delivery rows that may be unacknowledged at a time.
pub(crate) const WINDOW: usize = 2;

/// People per delivery row.
const CONTACTS_PER_ROW: u32 = 200;

/// Chats per delivery row.
const CHATS_PER_ROW: usize = 50;

/// A delivery row found unacknowledged at more delivery starts than this is taken back.
pub(crate) const MAX_STARTS: u32 = 3;

/// The running time without any acknowledgement after which the unsettled rows are taken
/// back.
pub(crate) const IDLE: Duration = Duration::from_secs(10 * 60);

/// How often the running time without an acknowledgement is stored.
const IDLE_STORED_EVERY: Duration = Duration::from_secs(30);

/// Takings back in a row, without an acknowledgement between them, that end the transfer
/// as incomplete.
pub(crate) const MAX_STRIKES: u32 = 3;

/// Single units (a person, a chat, a message) the core refused in a row, with no row that is
/// not the bare end of a chat acknowledged in between, after which the transfer ends as
/// incomplete: a core that refuses what it is given is not going to take the rest.
pub(crate) const MAX_REFUSED_RUN: u32 = 20;

/// The most RUNNING time a whole delivery may spend waiting for an acknowledgement that came
/// no more (every taking back adds the time it waited), after which the transfer ends as
/// incomplete. Nothing resets it: each message the core cannot store costs two waits of
/// [`IDLE`] (its row, then itself), and an acknowledgement of the rows around it must not
/// make the next one free. Kourier's own bound: receiving stays held back for at most this
/// much stuck time, however many messages the core cannot store.
pub(crate) const STUCK: Duration = Duration::from_secs(60 * 60);

/// Chunks in a row that did not begin where the stored cursor stands, after which the
/// delivery gives up (a fault of this program; it must not hold receiving back for good).
const MAX_OUT_OF_STEP: u32 = 3;

/// The bounds of a delivery; tests shorten them.
#[derive(Clone, Copy, Debug)]
pub(crate) struct DeliveryBounds {
    /// Messages per row.
    pub(crate) max_messages: usize,
    /// Staged bytes per row.
    pub(crate) max_bytes: u64,
    /// Unacknowledged rows at a time.
    pub(crate) window: usize,
    /// [`MAX_STARTS`].
    pub(crate) max_starts: u32,
    /// [`IDLE`].
    pub(crate) idle: Duration,
    /// [`IDLE_STORED_EVERY`].
    pub(crate) idle_stored_every: Duration,
    /// [`MAX_STRIKES`].
    pub(crate) max_strikes: u32,
    /// [`MAX_REFUSED_RUN`].
    pub(crate) max_refused: u32,
    /// [`STUCK`].
    pub(crate) stuck: Duration,
}

impl DeliveryBounds {
    /// What the app uses.
    pub(crate) const DEFAULT: Self = Self {
        max_messages: MAX_HISTORY_BATCH,
        max_bytes: MAX_ROW_BYTES,
        window: WINDOW,
        max_starts: MAX_STARTS,
        idle: IDLE,
        idle_stored_every: IDLE_STORED_EVERY,
        max_strikes: MAX_STRIKES,
        max_refused: MAX_REFUSED_RUN,
        stuck: STUCK,
    };
}

const PHASE_CONTACTS: i64 = 0;
const PHASE_CHATS: i64 = 1;
const PHASE_MESSAGES: i64 = 2;

const KIND_CONTACTS: i64 = 0;
const KIND_CHATS: i64 = 1;
const KIND_MESSAGES: i64 = 2;
/// The end of a chat whose last row was dropped, owed on its own: an empty batch that says
/// nothing follows. (The value was the kind of the "unread count" piece until step S3a.)
const KIND_END: i64 = 3;
/// The phone's pinned chats, one `PinnedConversations` in a row of its own after every
/// chat's upsert (step S4a). A single unit: when the core refuses it, it is dropped.
const KIND_PINS: i64 = 4;

/// The stored cursor (migration 0019).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Cursor {
    phase: i64,
    pos: i64,
    n: i64,
}

impl Cursor {
    #[cfg(any(test, feature = "testing"))]
    const START: Self = Self {
        phase: PHASE_CONTACTS,
        pos: 0,
        n: -1,
    };
}

/// A piece of the stage that was taken back and is appended again as singles.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Piece {
    id: i64,
    kind: i64,
    chat: i64,
    after_n: i64,
    through_n: i64,
    ends_chat: bool,
}

/// Where a feeder starts: one consistent look at `backend.db`.
#[derive(Clone, Debug)]
struct FeedStart {
    cursor: Cursor,
    redo: Vec<Piece>,
    delivered: u64,
}

/// Where a chunk stands in the delivery.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Place {
    /// The next piece after the cursor.
    Advance {
        /// Where the cursor must stand for this chunk.
        before: Cursor,
        /// Where it stands after it.
        after: Cursor,
    },
    /// One single of a piece that is appended again.
    Redo {
        piece: i64,
        /// The piece's `after_n` this chunk continues from.
        before: i64,
        /// The piece ends with this chunk.
        last: bool,
    },
}

/// What a chunk carries.
enum Payload {
    Contacts(Vec<StagedContact>),
    /// Chats with whether the peer is blocked (false for a group and Note to Self: a group's
    /// own flag is read where its row is made, `group_event`).
    Chats(Vec<(StagedChatRow, bool)>),
    Messages {
        conv: ConvKey,
        messages: Vec<IncomingMessage>,
        has_more: bool,
        /// `delivered / total` after this chunk.
        progress: Option<f32>,
        /// The messages of the shown chats in all (0 in a redo, which does not know it): the
        /// count `AccountProgress` reports against.
        total: u64,
    },
    /// Nothing follows for the chat (its last row was dropped).
    End {
        conv: ConvKey,
    },
    /// The pinned chats, in the phone's order.
    Pins(Vec<ConvKey>),
}

/// One delivery row to be: a typed piece of the stage.
struct Chunk {
    place: Place,
    kind: i64,
    /// `chats.idx` for messages and the end of a chat.
    chat: i64,
    after_n: i64,
    through_n: i64,
    ends_chat: bool,
    single: bool,
    payload: Payload,
}

impl Chunk {
    fn units(&self) -> u64 {
        match &self.payload {
            Payload::Contacts(list) => list.len() as u64,
            Payload::Chats(list) => list.len() as u64,
            Payload::Messages { messages, .. } => messages.len() as u64,
            Payload::End { .. } | Payload::Pins(_) => 1,
        }
    }
}

/// What the feeder hands over.
enum Fed {
    Chunk(Box<Chunk>),
    /// Everything after the cursor was handed over; `whole` says that the rows the feeder
    /// read agree with the stage's own counters.
    End {
        whole: bool,
    },
    /// The stage cannot be read.
    Failed(StageError),
}

/// How a delivery ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Delivered {
    /// Every row was acknowledged.
    Done {
        /// The frames the archive held.
        frames: u64,
    },
    /// The stage is lost; its record is taken back and the archive must be read again.
    StageLost,
    /// The delivery cannot finish (the bounds).
    Incomplete,
    /// `backend.db` failed.
    Internal,
}

/// What [`rebuild`] and [`forget`] reset of the cursor. `deliver_stuck_ms` stays on purpose
/// in [`forget`] (module docs, the migration).
const RESET_CURSOR: &str = "deliver_phase = 0, deliver_pos = 0, deliver_n = -1,
    delivered_messages = 0, deliver_idle_ms = 0, deliver_strikes = 0, deliver_refused = 0";

/// Takes the record of the stage back and sets the cursor to its start: the stage is lost
/// (or was read for a core that is gone), and the archive is read again from its first
/// frame. The read is marked open, so that read counts as a start (`note_read_start`).
/// `deliver_stuck_ms` stays (module docs): the total of the stuck time is the delivery's,
/// however many times the stage was read.
/// The delivery rows that are still in `core_outbox` stay there and reach the core; they
/// are no longer waited for.
pub(crate) fn forget(conn: &Connection, now: i64) -> Result<()> {
    conn.execute(
        &format!(
            "UPDATE history_transfer
                SET import_staged = 0, stage_messages = 0, stage_chats = 0, stage_frames = 0,
                    read_open = 1, {RESET_CURSOR}, updated_at = ?1
              WHERE id = 1 AND state = 'reading'"
        ),
        [now],
    )?;
    conn.execute("DELETE FROM history_delivery", [])?;
    Ok(())
}

/// A start with a rebuilt `core.db` (`StartMode::Rebuild`): the new core has nothing of the
/// import, so an import that is being delivered starts over as a whole, and whatever comes
/// of it is no longer "incomplete" (the core is empty).
pub(crate) fn rebuild(conn: &Connection, now: i64) -> Result<bool> {
    let touched = conn.execute(
        &format!(
            "UPDATE history_transfer
                SET import_staged = 0, stage_messages = 0, stage_chats = 0, stage_frames = 0,
                    read_open = 1, {RESET_CURSOR}, deliver_began = 0,
                    deliver_stuck_ms = 0, updated_at = ?1
              WHERE id = 1 AND state = 'reading' AND (import_staged = 1 OR deliver_began = 1)"
        ),
        [now],
    )?;
    conn.execute("DELETE FROM history_delivery", [])?;
    Ok(touched > 0)
}

/// Whether the core was given anything of this import.
pub(crate) fn began(conn: &Connection) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT deliver_began = 1 FROM history_transfer WHERE id = 1",
            [],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(false))
}

fn load_cursor(conn: &Connection) -> Result<Cursor> {
    Ok(conn.query_row(
        "SELECT deliver_phase, deliver_pos, deliver_n FROM history_transfer WHERE id = 1",
        [],
        |r| {
            Ok(Cursor {
                phase: r.get(0)?,
                pos: r.get(1)?,
                n: r.get(2)?,
            })
        },
    )?)
}

fn load_start(conn: &Connection) -> Result<FeedStart> {
    let cursor = load_cursor(conn)?;
    let delivered: i64 = conn.query_row(
        "SELECT delivered_messages FROM history_transfer WHERE id = 1",
        [],
        |r| r.get(0),
    )?;
    let redo = conn
        .prepare(
            "SELECT id, kind, chat, after_n, through_n, ends_chat FROM history_delivery
              WHERE seq IS NULL ORDER BY id",
        )?
        .query_map([], |r| {
            Ok(Piece {
                id: r.get(0)?,
                kind: r.get(1)?,
                chat: r.get(2)?,
                after_n: r.get(3)?,
                through_n: r.get(4)?,
                ends_chat: r.get(5)?,
            })
        })?
        .collect::<kourier_db::rusqlite::Result<_>>()?;
    Ok(FeedStart {
        cursor,
        redo,
        delivered: to_u64(delivered),
    })
}

/// The feeder (blocking): everything from `start` on, as chunks into `out`. Ends when the
/// receiver is gone.
fn feed(
    dir: &std::path::Path,
    key: &StageKey,
    recorded: Recorded,
    start: &FeedStart,
    bounds: DeliveryBounds,
    out: &mpsc::Sender<Fed>,
) {
    let end = match feed_all(dir, key, recorded, start, bounds, out) {
        Ok(Some(whole)) => Fed::End { whole },
        // The receiver is gone.
        Ok(None) => return,
        Err(e) => Fed::Failed(e),
    };
    let _ = out.blocking_send(end);
}

/// `Ok(None)`: the receiver is gone.
fn feed_all(
    dir: &std::path::Path,
    key: &StageKey,
    recorded: Recorded,
    start: &FeedStart,
    bounds: DeliveryBounds,
    out: &mpsc::Sender<Fed>,
) -> std::result::Result<Option<bool>, StageError> {
    let reader = StageReader::open(dir, key)?;
    let totals = reader.totals()?;
    if totals.messages != recorded.messages
        || totals.chats != recorded.chats
        || reader.frames()? != recorded.frames
    {
        // Not the stage that was recorded.
        return Err(StageError::Unreadable);
    }
    let shown: Vec<StagedChatRow> = reader
        .chats()?
        .into_iter()
        .filter(StagedChatRow::shown)
        .collect();
    let total: u64 = shown.iter().map(|chat| chat.counters.n_rows).sum();
    let send = |chunk: Chunk| out.blocking_send(Fed::Chunk(Box::new(chunk))).is_ok();
    // The chats are UPSERTED the least recent first and FILLED the most recent first. The
    // core lists chats by their newest row and, among chats without a row, the one it was
    // told of last on top (`kourier-core/src/query.rs`, the list's order): told in this
    // order, the list stands in the phone's order of recency before the first batch
    // arrives, and a chat that fills never changes its place.
    let upsert_at = |place: usize| {
        shown
            .len()
            .checked_sub(place + 1)
            .and_then(|i| shown.get(i))
    };
    // Whether the chat's contact is blocked; a group says it itself (`group_event`).
    let blocked_peer = |chat: &StagedChatRow| -> std::result::Result<bool, StageError> {
        let Some(peer) = chat.op.peer() else {
            return Ok(false);
        };
        let row = reader.contacts(peer.checked_sub(1), 1)?;
        Ok(row.first().is_some_and(|(n, c)| *n == peer && c.blocked))
    };

    // First what was taken back, one unit per row.
    for piece in &start.redo {
        let mut after = piece.after_n;
        match piece.kind {
            KIND_CONTACTS => {
                while after < piece.through_n {
                    let rows = reader.contacts(u32::try_from(after).ok(), 1)?;
                    let Some((n, contact)) = rows.into_iter().next() else {
                        return Err(StageError::Unreadable);
                    };
                    let through = i64::from(n);
                    let chunk = Chunk {
                        place: Place::Redo {
                            piece: piece.id,
                            before: after,
                            last: through >= piece.through_n,
                        },
                        kind: KIND_CONTACTS,
                        chat: 0,
                        after_n: after,
                        through_n: through,
                        ends_chat: false,
                        single: true,
                        payload: Payload::Contacts(vec![contact]),
                    };
                    if !send(chunk) {
                        return Ok(None);
                    }
                    after = through;
                }
            }
            KIND_CHATS => {
                while after < piece.through_n {
                    let through = after + 1;
                    let Some(chat) = usize::try_from(after).ok().and_then(upsert_at) else {
                        return Err(StageError::Unreadable);
                    };
                    let chunk = Chunk {
                        place: Place::Redo {
                            piece: piece.id,
                            before: after,
                            last: through >= piece.through_n,
                        },
                        kind: KIND_CHATS,
                        chat: 0,
                        after_n: after,
                        through_n: through,
                        ends_chat: false,
                        single: true,
                        payload: Payload::Chats(vec![(chat.clone(), blocked_peer(chat)?)]),
                    };
                    if !send(chunk) {
                        return Ok(None);
                    }
                    after = through;
                }
            }
            KIND_MESSAGES | KIND_END => {
                let Some(chat) = shown
                    .iter()
                    .find(|chat| i64::from(chat.counters.idx) == piece.chat)
                else {
                    return Err(StageError::Unreadable);
                };
                if piece.kind == KIND_END {
                    // The end of a chat whose last message was dropped.
                    let chunk = Chunk {
                        place: Place::Redo {
                            piece: piece.id,
                            before: piece.after_n,
                            last: true,
                        },
                        kind: KIND_END,
                        chat: piece.chat,
                        after_n: piece.after_n,
                        through_n: piece.through_n,
                        ends_chat: false,
                        single: true,
                        payload: Payload::End {
                            conv: chat.conv.clone(),
                        },
                    };
                    if !send(chunk) {
                        return Ok(None);
                    }
                    continue;
                }
                while after < piece.through_n {
                    let rows = reader.items(chat.counters.idx, u64::try_from(after).ok(), 1, 0)?;
                    let Some(item) = rows.into_iter().next() else {
                        return Err(StageError::Unreadable);
                    };
                    let through = to_i64(item.n);
                    let last = through >= piece.through_n;
                    let ends = last && piece.ends_chat;
                    let chunk = Chunk {
                        place: Place::Redo {
                            piece: piece.id,
                            before: after,
                            last,
                        },
                        kind: KIND_MESSAGES,
                        chat: piece.chat,
                        after_n: after,
                        through_n: through,
                        ends_chat: ends,
                        single: true,
                        payload: Payload::Messages {
                            conv: chat.conv.clone(),
                            messages: vec![item.msg],
                            has_more: !ends,
                            progress: None,
                            total: 0,
                        },
                    };
                    if !send(chunk) {
                        return Ok(None);
                    }
                    after = through;
                }
            }
            _ => return Err(StageError::Unreadable),
        }
    }

    // Then on from the cursor.
    let mut cursor = start.cursor;
    if cursor.phase == PHASE_CONTACTS {
        loop {
            let after = u32::try_from(cursor.pos - 1).ok();
            let rows = reader.contacts(after, CONTACTS_PER_ROW)?;
            let Some((last, _)) = rows.last() else {
                break;
            };
            let next = Cursor {
                phase: PHASE_CONTACTS,
                pos: i64::from(*last) + 1,
                n: -1,
            };
            let chunk = Chunk {
                place: Place::Advance {
                    before: cursor,
                    after: next,
                },
                kind: KIND_CONTACTS,
                chat: 0,
                after_n: cursor.pos - 1,
                through_n: i64::from(*last),
                ends_chat: false,
                single: rows.len() == 1,
                payload: Payload::Contacts(rows.into_iter().map(|(_, c)| c).collect()),
            };
            if !send(chunk) {
                return Ok(None);
            }
            cursor = next;
        }
        // The step into the next phase rides on the first chunk of it: the feeder only
        // names where the cursor must stand.
        let next = Cursor {
            phase: PHASE_CHATS,
            pos: 0,
            n: -1,
        };
        if !send(step(cursor, next)) {
            return Ok(None);
        }
        cursor = next;
    }
    if cursor.phase == PHASE_CHATS {
        while let Some(rows) = usize::try_from(cursor.pos)
            .ok()
            .filter(|from| *from < shown.len())
            .map(|from| {
                (from..shown.len().min(from + CHATS_PER_ROW))
                    .filter_map(upsert_at)
                    .collect::<Vec<_>>()
            })
            .filter(|rows| !rows.is_empty())
        {
            let next = Cursor {
                phase: PHASE_CHATS,
                pos: cursor.pos + to_i64(rows.len() as u64),
                n: -1,
            };
            let mut chats = Vec::with_capacity(rows.len());
            for chat in &rows {
                chats.push(((*chat).clone(), blocked_peer(chat)?));
            }
            let chunk = Chunk {
                place: Place::Advance {
                    before: cursor,
                    after: next,
                },
                kind: KIND_CHATS,
                chat: 0,
                after_n: cursor.pos,
                through_n: next.pos,
                ends_chat: false,
                single: rows.len() == 1,
                payload: Payload::Chats(chats),
            };
            if !send(chunk) {
                return Ok(None);
            }
            cursor = next;
        }
        let next = Cursor {
            phase: PHASE_MESSAGES,
            pos: 0,
            n: -1,
        };
        // The step into the messages carries the pins: ONE list after every chat's upsert
        // (the core makes a bare chat of every key the list names that it does not know),
        // naming only chats that are upserted, in the phone's order. `pinnedOrder` is
        // ascending and may have gaps: sorted, never used as an index.
        let mut pinned: Vec<&StagedChatRow> =
            shown.iter().filter(|chat| chat.pinned.is_some()).collect();
        pinned.sort_by_key(|chat| (chat.pinned, chat.counters.idx));
        let chunk = if pinned.is_empty() {
            step(cursor, next)
        } else {
            Chunk {
                place: Place::Advance {
                    before: cursor,
                    after: next,
                },
                kind: KIND_PINS,
                chat: 0,
                after_n: 0,
                through_n: 0,
                ends_chat: false,
                single: true,
                payload: Payload::Pins(pinned.into_iter().map(|chat| chat.conv.clone()).collect()),
            }
        };
        if !send(chunk) {
            return Ok(None);
        }
        cursor = next;
    }
    let mut delivered = start.delivered;
    let mut whole = true;
    while let Some(chat) = usize::try_from(cursor.pos).ok().and_then(|i| shown.get(i)) {
        let idx = chat.counters.idx;
        let read = |after: i64| {
            reader.items(
                idx,
                u64::try_from(after).ok(),
                bounds.max_messages.clamp(1, MAX_HISTORY_BATCH),
                bounds.max_bytes,
            )
        };
        let from_first = cursor.n < 0;
        let mut rows_of_chat = 0u64;
        // One batch ahead, to know whether a batch is the chat's last.
        let mut batch = read(cursor.n)?;
        loop {
            let through = batch.last().map_or(cursor.n, |item| to_i64(item.n));
            let ahead = if batch.is_empty() {
                Vec::new()
            } else {
                read(through)?
            };
            let ends = ahead.is_empty();
            rows_of_chat += batch.len() as u64;
            delivered += batch.len() as u64;
            let next = if ends {
                Cursor {
                    phase: PHASE_MESSAGES,
                    pos: cursor.pos + 1,
                    n: -1,
                }
            } else {
                Cursor {
                    phase: PHASE_MESSAGES,
                    pos: cursor.pos,
                    n: through,
                }
            };
            #[allow(clippy::cast_precision_loss, reason = "a fraction for a progress bar")]
            let progress = (total > 0).then(|| (delivered as f32 / total as f32).min(1.0));
            let chunk = Chunk {
                place: Place::Advance {
                    before: cursor,
                    after: next,
                },
                kind: KIND_MESSAGES,
                chat: i64::from(idx),
                after_n: cursor.n,
                through_n: through,
                ends_chat: ends,
                single: batch.len() <= 1,
                payload: Payload::Messages {
                    conv: chat.conv.clone(),
                    messages: batch.into_iter().map(|item| item.msg).collect(),
                    has_more: !ends,
                    progress,
                    total,
                },
            };
            if !send(chunk) {
                return Ok(None);
            }
            cursor = next;
            if ends {
                break;
            }
            batch = ahead;
        }
        // A chat read from its first row must hold what its counter says.
        whole &= !from_first || rows_of_chat == chat.counters.n_rows;
    }
    Ok(Some(whole))
}

/// A chunk that carries nothing and moves the cursor into the next phase.
fn step(before: Cursor, after: Cursor) -> Chunk {
    Chunk {
        place: Place::Advance { before, after },
        kind: KIND_CONTACTS,
        chat: 0,
        after_n: 0,
        through_n: 0,
        ends_chat: false,
        single: false,
        payload: Payload::Contacts(Vec::new()),
    }
}

/// The service id a person's row of `peers` is kept under: the ACI, else the PNI.
fn service_id_of(contact: &StagedContact) -> Option<ServiceId> {
    contact
        .aci
        .map(|aci| ServiceId::from(Aci::from(aci)))
        .or_else(|| contact.pni.map(|pni| ServiceId::from(Pni::from(pni))))
}

/// A person of the import as the core is told. `fetched`: the person's profile was fetched
/// before; then the archive's profile name is left out (a stage that is read again must not
/// put an old name over a fetched one). Only what the archive states is set.
fn contact_patch(contact: &StagedContact, fetched: bool) -> ContactPatch {
    let mut patch = ContactPatch::new(contact.user.clone());
    if let Some(name) = contact.profile_name.clone().filter(|_| !fetched) {
        patch.profile_name = Patch::Set(name);
    }
    if let Some(name) = contact.system_name.clone() {
        patch.system_name = Patch::Set(name);
    }
    if let Some(name) = contact.nickname.clone() {
        patch.nickname = Patch::Set(name);
    }
    if contact.blocked {
        patch.blocked = Some(true);
    }
    if contact.hidden {
        patch.hidden = Some(true);
    }
    if let Some(at) = contact.unregistered_at {
        patch.unregistered_at = Patch::Set(at);
    }
    if let Some(number) = contact.e164.clone() {
        patch.identifiers = Some(vec![Identifier {
            kind: IdentifierKind::Phone,
            value: number,
        }]);
    }
    patch
}

/// A person of the import: this backend's own rows, as the live path writes them when it
/// first meets a person (`storage::merge_contact`, `groups::apply`), and the event for the
/// core. A flag the archive does not state is left as it is (the syncs that follow the
/// transfer own it).
fn contact_event(tx: &Tx<'_>, contact: &StagedContact, now: i64) -> Result<BackendEvent> {
    let sid = service_id_of(contact).map(|id| id.service_id_string());
    let mut fetched = false;
    if let Some(sid) = &sid {
        peers::ensure(tx, sid, now)?;
        if let (Some(aci), Some(key)) = (contact.aci, contact.profile_key.as_ref()) {
            let key = Zeroizing::new(*key);
            if peers::store_profile_key(tx, Aci::from(aci), key.as_slice(), now)? {
                profile::request(tx, sid, now)?;
            }
        }
        if contact.profile_sharing {
            peers::set_profile_sharing(tx, sid, true, now)?;
        }
        if contact.system_name.is_some() {
            peers::set_system_contact(tx, sid, true, now)?;
        }
        if contact.blocked {
            peers::set_blocked(tx, sid, true, now)?;
        }
        if contact.unregistered_at.is_some() {
            peers::mark_unregistered(tx, sid, now)?;
        }
        fetched = profile::fetched(tx, sid)?;
    }
    Ok(BackendEvent::ContactUpsert(contact_patch(contact, fetched)))
}

/// The features of the chat of a person known only by a number: nothing can be sent there
/// (this backend sends to an ACI or a PNI), and the chat says so instead of offering a
/// composer that fails. Every other switch is the most restrictive default.
fn number_only_features() -> ConversationFeatures {
    ConversationFeatures {
        cannot_send: Some(CannotSend::NotAccepted),
        ..ConversationFeatures::default()
    }
}

/// The upsert of one shown chat (its features are put in front by the caller). What the
/// chat frame says is set where it differs from a new chat's default (the core's chat is
/// new: an import runs into an account without chats); the Storage Service sync that
/// follows the transfer owns these flags and wins where it differs (`storage`, D10). The
/// pinned order is not here: the pins come as one list after every upsert.
fn chat_event(chat: &StagedChatRow, blocked: bool) -> BackendEvent {
    let mut patch = ConversationPatch::new(chat.conv.clone());
    if chat.op.is_self() {
        patch.is_self = Some(true);
    }
    if blocked {
        patch.blocked = Some(true);
    }
    let frame = &chat.op.frame;
    if frame.archived {
        patch.archived = Some(true);
    }
    if frame.marked_unread {
        patch.marked_unread = Some(true);
    }
    if let Some(until) = frame.muted_until {
        patch.muted_until = Patch::Set(until);
    }
    if let ConvKey::Direct(user) = &chat.conv
        && user.scheme() == "e164"
    {
        patch.features = Some(number_only_features());
    }
    BackendEvent::ConversationUpsert(patch)
}

/// The upsert of a shown GROUP chat, with its features, and the group's `groups` row
/// (design 2.2, the lead's decisions of step S4b), in the chats zone:
///
/// - the row is written WITHOUT the phone's snapshot (`state` stays NULL: sending needs the
///   server's member list, and the first fetch is then a "first" one that replaces title,
///   members and timer and makes no notices), with every flag column the Storage Service
///   merge compares (blocked, accepted, archived, marked unread, muted until by the shared
///   conversion), `membership` as the archive's snapshot says where the row has none yet
///   ('pending' for an invitation, 'requesting' for a request to join, 'left'; a member or an
///   empty snapshot stays 'unknown', "still loading") and `features_hash` of the features
///   sent (the core has the group). A verdict the server already gave ('left', 'gone')
///   stands over the archive's word, and a redo never offers an invitation again after it;
/// - NEVER OVER THE SERVER'S STATE: when the row has a state already (a refresh ran during
///   the delivery, or after a rebuild), the row's flags are left alone and the upsert carries
///   what that state says (its title, members and timer, so that a rebuilt core gets a whole
///   group whatever the refresh does) and never the archive's;
/// - a `GroupRefresh` is queued for every group, a left one too (D10). The caller wakes the
///   job runner when the zone commits.
fn group_event(
    tx: &Tx<'_>,
    account: &crate::db::Account,
    chat: &StagedChatRow,
    group: &super::stage::StagedGroup,
    now: i64,
) -> Result<BackendEvent> {
    use super::stage::GroupMembership;
    use crate::groups::{self, GroupKeys, Membership};
    let keys = GroupKeys::from_master(group.master_key.0);
    groups::ensure(tx, &keys, now)?;
    let Some(row) = groups::load_by_master(tx, &keys.master)? else {
        return Err(Error::Internal("a group row vanished"));
    };
    let frame = &chat.op.frame;
    let mut patch = ConversationPatch::new(keys.conv_key());
    let membership = match &row.snapshot {
        None => {
            // Where the server has not said anything yet, the archive's word is our place.
            let membership = match row.membership {
                Membership::Unknown => match group.membership {
                    GroupMembership::Invited => Membership::Pending,
                    GroupMembership::Requesting => Membership::Requesting,
                    GroupMembership::Left => Membership::Left,
                    GroupMembership::Member | GroupMembership::Unknown => Membership::Unknown,
                },
                decided => decided,
            };
            tx.prepare_cached(
            "UPDATE groups SET blocked = ?2, whitelisted = ?3, archived = ?4, marked_unread = ?5,
               muted_until = ?6, membership = ?7, updated_at = ?8
             WHERE master_key = ?1",
        )?
        .execute((
            &keys.master[..],
            group.blocked,
            group.whitelisted,
            frame.archived,
            frame.marked_unread,
            frame.muted_until,
            membership.as_str(),
            now,
        ))?;
            if let Some(title) = &group.title {
                patch.title = Patch::Set(title.clone());
            }
            // An empty snapshot says nothing of the members: an empty list would wipe one.
            if let Some(members) = &group.members {
                patch.members = Some(kourier_model::MemberList::snapshot(members.clone()));
            }
            // Display only until the first fetch: a send takes the snapshot's timer, and sending
            // waits for the snapshot anyway. No version for a group.
            if let Some(secs) = frame.timer_secs {
                patch.timer = Patch::Set(Duration::from_secs(u64::from(secs)));
            }
            if group.blocked {
                patch.blocked = Some(true);
            }
            if frame.archived {
                patch.archived = Some(true);
            }
            if frame.marked_unread {
                patch.marked_unread = Some(true);
            }
            if let Some(until) = frame.muted_until {
                patch.muted_until = Patch::Set(until);
            }
            // Invited, as on the phone: the chat offers the invitation while the row says
            // 'pending', and no longer once the first fetch found it over ('left', 'gone': a redo
            // after a 403 must not offer it again). The first fetch corrects it too
            // (`groups::apply`).
            if group.membership == GroupMembership::Invited {
                patch.request_pending = Some(membership == Membership::Pending);
            }
            membership
        }
        Some(snapshot) => {
            // The server's state stands: its title, members and timer, and only what the row
            // implies of the flags.
            patch.title = if snapshot.title.is_empty() {
                Patch::Clear
            } else {
                Patch::Set(snapshot.title.clone())
            };
            patch.members = Some(crate::groups::apply::member_list(snapshot));
            patch.timer = if snapshot.timer_secs > 0 {
                Patch::Set(Duration::from_secs(u64::from(snapshot.timer_secs)))
            } else {
                Patch::Clear
            };
            patch.blocked = Some(row.blocked);
            patch.archived = Some(row.archived);
            patch.marked_unread = Some(row.marked_unread);
            patch.muted_until = row.muted_until.map_or(Patch::Clear, Patch::Set);
            patch.request_pending = Some(row.membership == Membership::Pending);
            row.membership
        }
    };
    let features = features::group_features(
        row.snapshot.as_ref(),
        membership,
        account,
        &features::Limits::load(tx)?,
    );
    tx.prepare_cached("UPDATE groups SET features_hash = ?2 WHERE master_key = ?1")?
        .execute((&keys.master[..], features::hash_value(&features)))?;
    patch.features = Some(features);
    crate::jobs::enqueue(
        tx,
        crate::jobs::JobKind::GroupRefresh,
        &groups::job_key(&keys.id),
        now,
        None,
    )?;
    Ok(BackendEvent::ConversationUpsert(patch))
}

#[cfg(test)]
thread_local! {
    /// Tests only: makes the NEXT check of a whole row in [`append`] fail. By data it cannot
    /// (every unit passed alone and the features beside a unit carry the unit's own key), so
    /// the branch is defence in depth, and only this switch reaches it.
    static FAIL_THE_NEXT_WHOLE_ROW: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether `events` pass the contract check the core runs on an envelope; gives them back.
fn valid(events: Vec<BackendEvent>) -> (Vec<BackendEvent>, bool) {
    let (envelope, _waiter) = Envelope::durable(events);
    let ok = envelope.validate().is_ok();
    (envelope.events, ok)
}

/// What a chunk became in its zone.
#[derive(Clone, Copy, Debug, Default)]
struct Appended {
    /// The row's seq, when there was something to append.
    seq: Option<i64>,
    /// Units the contract check refused.
    dropped: u64,
    /// Rows that were due under a confirmed clock.
    expired: u64,
    /// Group refreshes were queued (the job runner is woken when the zone commits).
    refreshes: bool,
}

/// The chunk's events, built inside its zone. Units that do not pass the contract check
/// are left out and counted: a refused row would be quarantined by the forwarder for good.
fn events_of<T: SignalTransport>(
    tx: &Tx<'_>,
    shared: &Shared<T>,
    chunk: Chunk,
    now: i64,
    done: &mut Appended,
) -> Result<Vec<BackendEvent>> {
    // A chunk of a row that was taken back: what the zone of that row recorded in `peers`
    // (the core has these chats' features) is not true, the core never took the row.
    let redo = matches!(chunk.place, Place::Redo { .. });
    let mut checked = |units: Vec<BackendEvent>| -> Vec<BackendEvent> {
        let (units, ok) = valid(units);
        if ok {
            return units;
        }
        let mut kept = Vec::with_capacity(units.len());
        for unit in units {
            let (mut unit, ok) = valid(vec![unit]);
            if ok {
                kept.append(&mut unit);
            } else {
                done.dropped += 1;
            }
        }
        kept
    };
    Ok(match chunk.payload {
        Payload::Contacts(contacts) => {
            let mut events = Vec::with_capacity(contacts.len());
            for contact in &contacts {
                events.push(contact_event(tx, contact, now)?);
            }
            checked(events)
        }
        Payload::Chats(chats) => {
            let Some(account) = shared.account() else {
                return Err(Error::Internal("a delivery without an account"));
            };
            let mut units = Vec::with_capacity(chats.len());
            let mut groups = Vec::new();
            for (chat, blocked) in &chats {
                units.push(match chat.op.group() {
                    Some(group) => {
                        groups.push(chat.conv.clone());
                        group_event(tx, &account, chat, group, now)?
                    }
                    None => chat_event(chat, *blocked),
                });
            }
            if !groups.is_empty() {
                done.refreshes = true;
            }
            let events = checked(units);
            // A group whose upsert did not pass is not in the core.
            for conv in &groups {
                let kept = events.iter().any(
                    |event| matches!(event, BackendEvent::ConversationUpsert(p) if &p.key == conv),
                );
                if !kept {
                    features::forget_group_features(tx, conv)?;
                }
            }
            // The 1:1 timer and its version, from the chat frame: into `peers` BEFORE the
            // features are computed, so that the chat's first upsert carries them
            // (`features_event`) and what Kourier sends there carries them too. The version
            // is written also when the timer is off. The PAIR is written only when its
            // version is not below the stored one (the incoming rule, `timer::incoming`):
            // this zone runs again after a redo or a lost stage, when the chat is already in
            // the core and the user may have changed its timer meanwhile (`timer::change`
            // stores a version above the archive's). The archive's old timer beside that
            // newer version would undo the user's change in what Kourier sends.
            for (chat, _) in &chats {
                if let ConvKey::Direct(user) = &chat.conv
                    && let Some(peer) = features::direct_service_id(&account, user)
                {
                    peers::set_expire_timer_unless_older(
                        tx,
                        &peer.service_id_string(),
                        chat.op.frame.timer_secs.unwrap_or(0),
                        chat.op.frame.timer_version.max(1),
                        now,
                    )?;
                }
            }
            // Chat by chat, each with its features right in front of it: the core makes a
            // chat at the first event that names it, and the order in which it makes them
            // is the order of the list until they fill (the feeder's `upsert_at`).
            let mut out = Vec::with_capacity(events.len() * 2);
            for event in events {
                if redo
                    && let BackendEvent::ConversationUpsert(patch) = &event
                    && let ConvKey::Direct(user) = &patch.key
                {
                    features::forget_features(tx, user)?;
                }
                out.extend(features::with_features(tx, &account, vec![event], now)?);
            }
            out
        }
        Payload::Messages {
            conv,
            mut messages,
            has_more,
            progress,
            ..
        } => {
            // A row that is due is not delivered, but only under a clock the server
            // confirmed in this run; otherwise it goes on with its expiry and the core's
            // policy for an unconfirmed clock deals with it.
            if shared.clock_confirmed.get() {
                let before = messages.len();
                messages.retain(|m| {
                    m.expiry
                        .as_ref()
                        .and_then(kourier_model::Expiry::expires_at)
                        .is_none_or(|at| at > now)
                });
                done.expired += (before - messages.len()) as u64;
            }
            let batch = |messages: Vec<IncomingMessage>| BackendEvent::HistoryBatch {
                conv: conv.clone(),
                messages,
                has_more,
                progress,
            };
            let mut events = Vec::with_capacity(2);
            if !messages.is_empty() {
                let (whole, ok) = valid(vec![batch(messages)]);
                if ok {
                    events = whole;
                } else {
                    // Cut into single messages; the ones that fail are dropped.
                    let Some(BackendEvent::HistoryBatch { messages, .. }) =
                        whole.into_iter().next()
                    else {
                        return Err(Error::Internal("a history batch"));
                    };
                    let mut kept = Vec::with_capacity(messages.len());
                    for message in messages {
                        let (single, ok) = valid(vec![batch(vec![message])]);
                        match single.into_iter().next() {
                            Some(BackendEvent::HistoryBatch { mut messages, .. }) if ok => {
                                kept.append(&mut messages);
                            }
                            _ => done.dropped += 1,
                        }
                    }
                    if !kept.is_empty() {
                        events.push(batch(kept));
                    }
                }
            }
            if events.is_empty() && !has_more {
                // The chat's last row was not delivered (it was due, or it is not valid):
                // every batch the core heard of the chat said more follows, so say that
                // nothing does. The contract allows an empty batch.
                events.push(batch(Vec::new()));
            }
            events
        }
        Payload::End { conv } => {
            // The last row of the chat was dropped: nothing follows.
            vec![BackendEvent::HistoryBatch {
                conv,
                messages: Vec::new(),
                has_more: false,
                progress: None,
            }]
        }
        // The full list: the core's pin list is idempotent (it unpins what the list does not
        // name and orders what it does), so a row appended again after a kill sets the
        // same pins. The core makes a bare chat of every key the list names that it does not
        // have, so a chat whose upsert was dropped for good is left out (`take_back` forgot
        // its features in the zone that dropped it: the records say the core never took it).
        // A chat known only by a number has no such record and stays named.
        Payload::Pins(keys) => {
            let mut kept = Vec::with_capacity(keys.len());
            let mut left_out = 0u64;
            for key in keys {
                if features::core_has_chat(tx, &key)? == Some(false) {
                    left_out += 1;
                    continue;
                }
                kept.push(key);
            }
            if left_out > 0 {
                tracing::warn!(
                    left_out,
                    "pinned chats the core never took are left out of the pins"
                );
            }
            if kept.is_empty() {
                Vec::new()
            } else {
                checked(vec![BackendEvent::PinnedConversations(kept)])
            }
        }
    })
}

/// THE ZONE OF ONE CHUNK (module docs): the look at the cursor, the backend rows, one
/// `core_outbox` row, its entry, the cursor. `Err(Error::Internal("out of step"))` when
/// the chunk does not begin where the stored state stands: nothing is written.
fn append<T: SignalTransport>(shared: &Shared<T>, chunk: Chunk, now: i64) -> Result<Appended> {
    let wake = shared.wake.clone();
    let jobs_wake = shared.jobs_wake.clone();
    shared.db.zone(|tx| -> Result<Appended> {
        // 1. Exactly once: the chunk continues the stored state, or it is not appended.
        match chunk.place {
            Place::Advance { before, .. } => {
                if load_cursor(tx)? != before {
                    return Err(Error::Internal("out of step"));
                }
            }
            Place::Redo { piece, before, .. } => {
                let stands: Option<i64> = tx
                    .query_row(
                        "SELECT after_n FROM history_delivery WHERE id = ?1 AND seq IS NULL",
                        [piece],
                        |r| r.get(0),
                    )
                    .optional()?;
                if stands != Some(before) {
                    return Err(Error::Internal("out of step"));
                }
            }
        }
        let (place, kind, chat) = (chunk.place, chunk.kind, chunk.chat);
        let (after_n, through_n) = (chunk.after_n, chunk.through_n);
        let (ends_chat, single, units) = (chunk.ends_chat, chunk.single, chunk.units());
        let is_messages = matches!(chunk.payload, Payload::Messages { .. });
        let is_contacts = matches!(chunk.payload, Payload::Contacts(_));
        // The chats a row of chats names, for the case that the row is dropped as a whole.
        let named: Vec<ConvKey> = match &chunk.payload {
            Payload::Chats(list) => list.iter().map(|(chat, _)| chat.conv.clone()).collect(),
            _ => Vec::new(),
        };
        let mut done = Appended::default();
        // 2. and 3. The backend rows and the events.
        let events = events_of(tx, shared, chunk, now, &mut done)?;
        // The row as a whole, as the core will check it (the features were put beside units
        // that passed alone). A row that fails here is not appended:
        // the forwarder would quarantine it for good.
        let events = if events.is_empty() {
            events
        } else {
            let (events, ok) = valid(events);
            #[cfg(test)]
            let ok = ok && !FAIL_THE_NEXT_WHOLE_ROW.replace(false);
            if ok {
                events
            } else {
                tracing::error!("a delivery row did not pass the contract check; dropped");
                done.dropped += units;
                // The zone set these chats' features as sent; the core never gets them.
                for conv in &named {
                    features::forget_chat_features(tx, conv)?;
                }
                Vec::new()
            }
        };
        if is_contacts || done.refreshes {
            // Profile fetches the people of this chunk need, and the refreshes of its
            // groups, run beside the import.
            let jobs_wake = jobs_wake.clone();
            tx.on_commit(move || jobs_wake.notify_one());
        }
        if !events.is_empty() {
            let seq = outbox::append_events(tx, &events, now, &wake)?;
            // 4. The entry of the row.
            tx.execute(
                "INSERT INTO history_delivery(seq, kind, chat, after_n, through_n, ends_chat,
                   single, messages) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                (
                    seq,
                    kind,
                    chat,
                    after_n,
                    through_n,
                    ends_chat,
                    single,
                    to_i64(units),
                ),
            )?;
            done.seq = Some(seq);
            // Not durable yet: a kill here is a rollback of the row, the entry and
            // everything else of this zone (tests: `AT_COMMIT` lies after the commit).
            kourier_db::failpoint::hit(FAILPOINT_INSIDE_CHUNK);
            tx.on_commit(|| kourier_db::failpoint::hit(FAILPOINT_AT_COMMIT));
        }
        // 5. The cursor (or the piece), and the counts.
        match place {
            Place::Advance { after, .. } => {
                tx.execute(
                    "UPDATE history_transfer
                        SET deliver_phase = ?1, deliver_pos = ?2, deliver_n = ?3,
                            delivered_messages = delivered_messages + ?4,
                            deliver_rows = deliver_rows + ?5,
                            deliver_dropped = deliver_dropped + ?6,
                            deliver_expired = deliver_expired + ?7,
                            deliver_began = max(deliver_began, ?5), updated_at = ?8
                      WHERE id = 1",
                    (
                        after.phase,
                        after.pos,
                        after.n,
                        if is_messages { to_i64(units) } else { 0 },
                        i64::from(done.seq.is_some()),
                        to_i64(done.dropped),
                        to_i64(done.expired),
                        now,
                    ),
                )?;
            }
            Place::Redo { piece, last, .. } => {
                if last {
                    tx.execute("DELETE FROM history_delivery WHERE id = ?1", [piece])?;
                } else {
                    tx.execute(
                        "UPDATE history_delivery SET after_n = ?2 WHERE id = ?1",
                        (piece, through_n),
                    )?;
                }
                tx.execute(
                    "UPDATE history_transfer
                        SET deliver_rows = deliver_rows + ?1,
                            deliver_dropped = deliver_dropped + ?2,
                            deliver_expired = deliver_expired + ?3, updated_at = ?4
                      WHERE id = 1",
                    (
                        i64::from(done.seq.is_some()),
                        to_i64(done.dropped),
                        to_i64(done.expired),
                        now,
                    ),
                )?;
            }
        }
        Ok(done)
    })
}

/// What a look at the unsettled rows found.
#[derive(Clone, Copy, Debug, Default)]
struct Settled {
    /// Rows the core answered for (acknowledged or refused).
    answered: usize,
    /// Rows still waiting in `core_outbox`.
    waiting: usize,
    /// Pieces to append again were made by this look: the feeder must start anew.
    new_pieces: bool,
    /// Pieces to append again exist.
    pending: bool,
    /// The core has refused [`DeliveryBounds::max_refused`] single units in a row.
    refused_out: bool,
}

/// One unsettled delivery row.
struct Entry {
    id: i64,
    seq: i64,
    kind: i64,
    chat: i64,
    ends_chat: bool,
    single: bool,
    messages: i64,
    starts: u32,
    /// Still in `core_outbox`.
    waiting: bool,
    /// In `core_outbox_poison`.
    poisoned: bool,
}

fn entries(conn: &Connection) -> Result<Vec<Entry>> {
    Ok(conn
        .prepare(
            "SELECT d.id, d.seq, d.kind, d.chat, d.ends_chat, d.single, d.messages, d.starts,
               EXISTS (SELECT 1 FROM core_outbox o WHERE o.seq = d.seq),
               EXISTS (SELECT 1 FROM core_outbox_poison p WHERE p.seq = d.seq)
               FROM history_delivery d WHERE d.seq IS NOT NULL ORDER BY d.id",
        )?
        .query_map([], |r| {
            Ok(Entry {
                id: r.get(0)?,
                seq: r.get(1)?,
                kind: r.get(2)?,
                chat: r.get(3)?,
                ends_chat: r.get(4)?,
                single: r.get(5)?,
                messages: r.get(6)?,
                starts: r.get(7)?,
                waiting: r.get(8)?,
                poisoned: r.get(9)?,
            })
        })?
        .collect::<kourier_db::rusqlite::Result<_>>()?)
}

/// The zone of a row of chats recorded in `peers` that the core has these chats' features
/// (`features::features_event`). A row that is taken back is not delivered as it is: the
/// chats it named are not in the core, and `peers.features_hash` must not say they are,
/// whether the row is appended again as singles, dropped for good, or lost with a pending
/// piece by [`forget`]. Their hashes are cleared and nobody else's: the chats are read from
/// the row itself, in `core_outbox` or, when the core refused it, in `core_outbox_poison`.
/// (A redo forgets them again before it states them, [`events_of`].)
fn forget_features_of_row(tx: &Tx<'_>, seq: i64) -> Result<()> {
    let row: Option<Vec<u8>> = tx
        .query_row(
            "SELECT envelope FROM core_outbox WHERE seq = ?1
             UNION ALL SELECT envelope FROM core_outbox_poison WHERE seq = ?1 LIMIT 1",
            [seq],
            |r| r.get(0),
        )
        .optional()?;
    let Some(row) = row else {
        return Ok(());
    };
    // A row this version cannot read names nothing it could forget.
    let Ok(events) = decode_outbox_row(&row) else {
        return Ok(());
    };
    for event in events {
        if let BackendEvent::ConversationUpsert(patch) = event {
            features::forget_chat_features(tx, &patch.key)?;
        }
    }
    Ok(())
}

/// A row the core will not take as it is (refused, or not acknowledged within the bounds)
/// is taken back: a row of several units becomes a piece to append again as singles; a
/// single is dropped and counted (its copy in `core_outbox_poison`, if the core refused it,
/// goes: a refused history must not pile up in `backend.db` with its content), and when it
/// was the last row of its chat the chat's end is owed on its own (a piece of kind
/// [`KIND_END`]). Returns whether a piece to append again was made.
fn take_back(tx: &Tx<'_>, entry: &Entry) -> Result<bool> {
    if entry.kind == KIND_CHATS {
        forget_features_of_row(tx, entry.seq)?;
    }
    tx.execute("DELETE FROM core_outbox WHERE seq = ?1", [entry.seq])?;
    // The refused row's copy is not kept: its content is appended again, or dropped.
    tx.execute("DELETE FROM core_outbox_poison WHERE seq = ?1", [entry.seq])?;
    if entry.single {
        tx.execute("DELETE FROM history_delivery WHERE id = ?1", [entry.id])?;
        tx.execute(
            "UPDATE history_transfer SET deliver_dropped = deliver_dropped + ?1 WHERE id = 1",
            [entry.messages.max(1)],
        )?;
        tracing::warn!(
            kind = entry.kind,
            "a history unit the core would not take was dropped"
        );
        if entry.kind == KIND_MESSAGES && entry.ends_chat {
            tx.execute(
                "INSERT INTO history_delivery(seq, kind, chat, after_n, through_n, single)
                 VALUES (NULL, ?1, ?2, 0, 0, 1)",
                (KIND_END, entry.chat),
            )?;
            return Ok(true);
        }
        Ok(false)
    } else {
        tx.execute(
            "UPDATE history_delivery SET seq = NULL, starts = 0 WHERE id = ?1",
            [entry.id],
        )?;
        Ok(true)
    }
}

/// Looks at the unsettled rows: one that left `core_outbox` is settled, and its sequence
/// number is looked up in `core_outbox_poison` (a refused row is taken back). An answer of
/// the core, of either kind, ends the count of the time without one. Refused single units
/// are counted in a row; a row that is acknowledged (the bare end of a chat aside: it is
/// not what a core that refuses messages refuses) ends the run, and `max_refused` of them
/// end the delivery.
fn settle(db: &ZoneDb, max_refused: u32) -> Result<Settled> {
    let found = db.zone(|tx| -> Result<Settled> {
        let mut found = Settled::default();
        let mut run: u32 = tx.query_row(
            "SELECT deliver_refused FROM history_transfer WHERE id = 1",
            [],
            |r| r.get(0),
        )?;
        for entry in entries(tx)? {
            if entry.waiting {
                found.waiting += 1;
                continue;
            }
            found.answered += 1;
            if entry.poisoned {
                found.new_pieces |= take_back(tx, &entry)?;
                if entry.single {
                    run = run.saturating_add(1);
                }
            } else {
                tx.execute("DELETE FROM history_delivery WHERE id = ?1", [entry.id])?;
                if entry.kind != KIND_END {
                    run = 0;
                }
            }
        }
        if found.answered > 0 {
            tx.execute(
                "UPDATE history_transfer SET deliver_idle_ms = 0, deliver_strikes = 0,
                   deliver_refused = ?1 WHERE id = 1",
                [run],
            )?;
        }
        found.refused_out = run >= max_refused.max(1);
        found.pending = tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM history_delivery WHERE seq IS NULL)",
            [],
            |r| r.get(0),
        )?;
        Ok(found)
    })?;
    if found.answered > 0 {
        kourier_db::failpoint::hit(FAILPOINT_AFTER_SETTLE);
    }
    Ok(found)
}

/// A delivery start: the rows an earlier run left unacknowledged are counted (and one that
/// was found so too often is then taken back, [`strike`]).
fn note_start(db: &ZoneDb) -> Result<usize> {
    db.zone(|tx| -> Result<usize> {
        Ok(tx.execute(
            "UPDATE history_delivery SET starts = starts + 1
              WHERE seq IS NOT NULL AND seq IN (SELECT seq FROM core_outbox)",
            [],
        )?)
    })
}

/// What a taking back came to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Struck {
    /// Takings back in a row without an answer of the core, this one included (0: nothing
    /// was taken back).
    strikes: u32,
    /// 1 when the oldest waiting row was taken back (dropped if it is a single, split if it
    /// has several units), else 0. Rows of several units behind it that were split too are
    /// not counted.
    taken: usize,
    /// The running time all the takings back of this delivery waited, this one included.
    stuck: Duration,
    /// A piece to append again was made.
    piece: bool,
}

/// Takes back the OLDEST row that is still unacknowledged (`over_starts`: only if it was
/// found so at more than that many delivery starts) and counts a strike. Only the oldest
/// can be DROPPED: the forwarder ships in order and the core stops at the first failing job
/// of an envelope (`kourier-core/src/supervisor.rs`, `apply`: later jobs are skipped), so of
/// two waiting rows only the first can be what the core is stuck on; the one behind it was
/// never tried, and when it is a single, dropping it would lose a good message beside the
/// bad one. It goes next, if it is stuck too, at the next bound.
///
/// A waiting row of SEVERAL units behind the oldest is split into singles all the same (it
/// loses nothing: its content is appended again, and what the core already has is ignored).
/// Left whole, it would be shipped in one envelope with the first singles of the oldest
/// row's content, and when the bad message is the first of those, it could not be
/// acknowledged either, would be the oldest at the next bound, and would cost a strike for a
/// row that is not at fault.
///
/// `waited` is the running time spent without an acknowledgement; it is added to the total
/// nothing ever resets.
fn strike(db: &ZoneDb, over_starts: Option<u32>, waited: Duration) -> Result<Struck> {
    db.zone(|tx| -> Result<Struck> {
        let over = |entry: &Entry| over_starts.is_none_or(|max| entry.starts > max);
        let mut waiting = entries(tx)?.into_iter().filter(|entry| entry.waiting);
        let Some(oldest) = waiting.next().filter(over) else {
            return Ok(Struck::default());
        };
        let mut piece = take_back(tx, &oldest)?;
        for behind in waiting.filter(|entry| !entry.single && over(entry)) {
            piece |= take_back(tx, &behind)?;
        }
        let waited = to_i64(u64::try_from(waited.as_millis()).unwrap_or(u64::MAX));
        let (strikes, stuck_ms): (u32, i64) = tx.query_row(
            "UPDATE history_transfer SET deliver_strikes = deliver_strikes + 1,
               deliver_idle_ms = 0, deliver_stuck_ms = deliver_stuck_ms + ?1
              WHERE id = 1 RETURNING deliver_strikes, deliver_stuck_ms",
            [waited],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        Ok(Struck {
            strikes,
            taken: 1,
            stuck: Duration::from_millis(to_u64(stuck_ms)),
            piece,
        })
    })
}

/// The running time without an acknowledgement: what was stored of earlier runs and, on a
/// monotonic clock, this run since the last answer. The time the application was closed is
/// in neither (as the download's `Ran`).
struct Idle {
    before: Duration,
    since: tokio::time::Instant,
}

impl Idle {
    fn so_far(&self) -> Duration {
        self.before.saturating_add(self.since.elapsed())
    }

    fn reset(&mut self) {
        self.before = Duration::ZERO;
        self.since = tokio::time::Instant::now();
    }
}

fn store_idle<T: SignalTransport>(shared: &Shared<T>, idle: Duration) {
    let ms = to_i64(u64::try_from(idle.as_millis()).unwrap_or(u64::MAX));
    let stored = shared.db.zone(|tx| -> Result<usize> {
        Ok(tx.execute(
            "UPDATE history_transfer SET deliver_idle_ms = ?1 WHERE id = 1",
            [ms],
        )?)
    });
    if let Err(e) = stored {
        tracing::warn!(error = %e, "storing the delivery's idle time failed");
    }
}

/// A running feeder.
struct Feeder {
    rx: mpsc::Receiver<Fed>,
}

fn start_feeder<T: SignalTransport>(
    shared: &Shared<T>,
    key: &Zeroizing<[u8; 32]>,
    recorded: Recorded,
) -> Result<Feeder> {
    let start = shared.db.read(load_start)?;
    let bounds = shared.tuning.transfer_delivery;
    let (tx, rx) = mpsc::channel(bounds.window.max(1));
    let dir: PathBuf = shared.transfer.dir().to_owned();
    let key = key.clone();
    // Not awaited: it ends when its channel does (a stop drops the receiver).
    drop(tokio::task::spawn_blocking(move || {
        let key = StageKey::derive(&key);
        feed(&dir, &key, recorded, &start, bounds, &tx);
    }));
    Ok(Feeder { rx })
}

/// DELIVERY (module docs): the recorded stage to the core, from where the cursor stands,
/// until every row was acknowledged.
pub(crate) async fn deliver<T: SignalTransport>(
    shared: &Rc<Shared<T>>,
    key: &Zeroizing<[u8; 32]>,
    recorded: Recorded,
) -> Delivered {
    match run(shared, key, recorded).await {
        Ok(end) => end,
        Err(e) => {
            tracing::error!(error = %e, "the delivery of the history import failed");
            Delivered::Internal
        }
    }
}

async fn run<T: SignalTransport>(
    shared: &Rc<Shared<T>>,
    key: &Zeroizing<[u8; 32]>,
    recorded: Recorded,
) -> Result<Delivered> {
    let bounds = shared.tuning.transfer_delivery;
    note_start(&shared.db)?;
    let before: i64 = shared.db.read(|c| -> Result<i64> {
        Ok(c.query_row(
            "SELECT deliver_idle_ms FROM history_transfer WHERE id = 1",
            [],
            |r| r.get(0),
        )?)
    })?;
    let mut idle = Idle {
        before: Duration::from_millis(to_u64(before)),
        since: tokio::time::Instant::now(),
    };
    let struck = strike(&shared.db, Some(bounds.max_starts), idle.so_far())?;
    if struck.taken > 0 {
        tracing::warn!(
            rows = struck.taken,
            strikes = struck.strikes,
            "a delivery row stayed unacknowledged over too many starts; taken back"
        );
        idle.reset();
        if struck.strikes >= bounds.max_strikes || struck.stuck >= bounds.stuck {
            return Ok(Delivered::Incomplete);
        }
    }
    tracing::info!(
        messages = recorded.messages,
        chats = recorded.chats,
        "delivering the history import"
    );
    let mut acked = shared.acked.clone();
    let mut feeder = start_feeder(shared, key, recorded)?;
    let mut ended: Option<bool> = None;
    let mut stored_idle = tokio::time::Instant::now();
    let mut restart = false;
    let mut out_of_step = 0u32;
    // The pins' chunk, held until every row before it is settled (below).
    let mut held: Option<Box<Chunk>> = None;
    // The messages of the shown chats in all, as the feeder says (`AccountProgress`).
    let mut messages_total = 0u64;
    loop {
        // Back-pressure: fewer than the window of rows may be unsettled before the next
        // chunk is taken, and at the end none.
        let found = settle(&shared.db, bounds.max_refused)?;
        if found.answered > 0 {
            idle.reset();
        }
        if found.refused_out {
            tracing::warn!(
                refused = bounds.max_refused,
                "the core refused one unit of the history import after another; giving up"
            );
            return Ok(Delivered::Incomplete);
        }
        if found.new_pieces || restart {
            // Pieces were taken back: a new feeder cuts them into singles first, then goes
            // on from the cursor (which still stands before a held chunk: it comes again).
            feeder = start_feeder(shared, key, recorded)?;
            ended = None;
            restart = false;
            held = None;
        }
        let settled = found.waiting == 0 && !found.pending;
        let room = found.waiting < bounds.window.max(1);
        if let Some(whole) = ended
            && found.waiting == 0
            && !found.pending
        {
            if !whole {
                tracing::warn!("the stage holds other rows than its counters say");
                return lost(shared);
            }
            break;
        }
        // The pins go to the core only when every row before them is settled: a chats row
        // that the core refuses or does not take is taken back and appended again AFTER
        // anything that is already behind it, and the pins must not reach the core before
        // the chats they name (it would make bare chats of them).
        let next = match held.take() {
            Some(chunk) if settled => Some(Some(chunk)),
            Some(chunk) => {
                held = Some(chunk);
                None
            }
            None if room && ended.is_none() => Some(None),
            None => None,
        };
        if let Some(chunk) = next {
            let fed = match chunk {
                Some(chunk) => Some(Fed::Chunk(chunk)),
                None => feeder.rx.recv().await,
            };
            match fed {
                Some(Fed::Chunk(chunk)) if chunk.kind == KIND_PINS && !settled => {
                    held = Some(chunk);
                }
                Some(Fed::Chunk(chunk)) => {
                    kourier_db::failpoint::hit(FAILPOINT_BEFORE_CHUNK);
                    let now = shared.now();
                    let counted = match &chunk.payload {
                        Payload::Messages { total, .. } => {
                            messages_total = messages_total.max(*total);
                            true
                        }
                        _ => false,
                    };
                    match append(shared, *chunk, now) {
                        Ok(done) => {
                            out_of_step = 0;
                            if done.seq.is_some() {
                                kourier_db::failpoint::hit(FAILPOINT_AFTER_CHUNK);
                            }
                            if counted {
                                report_messages(shared, messages_total).await?;
                            }
                        }
                        Err(Error::Internal("out of step")) => {
                            // A chunk of a feeder the stored state has moved away from.
                            // A feeder that starts from the stored state cannot be out of
                            // step with it: when it is all the same, time after time, the
                            // delivery ends instead of turning in a circle for good.
                            out_of_step += 1;
                            if out_of_step > MAX_OUT_OF_STEP {
                                return Err(Error::Internal("the delivery cannot find its place"));
                            }
                            tracing::warn!("a delivery chunk was out of step; feeding anew");
                            feeder = start_feeder(shared, key, recorded)?;
                            continue;
                        }
                        Err(e) => return Err(e),
                    }
                    // Let the forwarder and the other tasks of the account run.
                    tokio::task::yield_now().await;
                }
                Some(Fed::End { whole }) => ended = Some(whole),
                Some(Fed::Failed(e)) => {
                    tracing::warn!(error = %e, "the recorded stage cannot be read");
                    return lost(shared);
                }
                None => {
                    tracing::warn!("the feeder of the history import ended without a word");
                    return lost(shared);
                }
            }
            continue;
        }
        // Wait for the core: an acknowledgement, or the next look at the bounds.
        let left = bounds.idle.saturating_sub(idle.so_far());
        if left.is_zero() {
            let struck = strike(&shared.db, None, idle.so_far())?;
            tracing::warn!(
                rows = struck.taken,
                strikes = struck.strikes,
                "the core acknowledged nothing of the history import for too long; its oldest \
                 waiting row is taken back"
            );
            idle.reset();
            stored_idle = tokio::time::Instant::now();
            if struck.strikes >= bounds.max_strikes || struck.stuck >= bounds.stuck {
                return Ok(Delivered::Incomplete);
            }
            restart = struck.piece;
            continue;
        }
        tokio::select! {
            changed = acked.changed() => {
                if changed.is_err() {
                    // The forwarder is gone: the account is going away, nothing will be
                    // acknowledged in this run, and this is no failure of the delivery.
                    std::future::pending::<()>().await;
                }
            }
            () = tokio::time::sleep(left.min(bounds.idle_stored_every)) => {
                if stored_idle.elapsed() >= bounds.idle_stored_every {
                    stored_idle = tokio::time::Instant::now();
                    store_idle(shared, idle.so_far());
                }
            }
        }
    }
    let (rows, dropped, expired): (i64, i64, i64) = shared.db.read(|c| -> Result<_> {
        Ok(c.query_row(
            "SELECT deliver_rows, deliver_dropped, deliver_expired FROM history_transfer
              WHERE id = 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?)
    })?;
    tracing::info!(
        rows,
        dropped,
        expired,
        messages = recorded.messages,
        "the history import is delivered: the core acknowledged every row"
    );
    kourier_db::failpoint::hit(FAILPOINT_BEFORE_DONE);
    Ok(Delivered::Done {
        frames: recorded.frames,
    })
}

/// Tells the UI how many messages of `total` are delivered (`progress.rs`): the stored count
/// after the chunk that was just appended.
async fn report_messages<T: SignalTransport>(shared: &Shared<T>, total: u64) -> Result<()> {
    if total == 0 || !shared.settling.is_on() {
        return Ok(());
    }
    let delivered: i64 = shared.db.read(|c| -> Result<i64> {
        Ok(c.query_row(
            "SELECT delivered_messages FROM history_transfer WHERE id = 1",
            [],
            |r| r.get(0),
        )?)
    })?;
    shared.settling.set_messages(to_u64(delivered), total);
    crate::progress::report(shared).await
}

/// The stage is lost: its record is taken back and the cursor reset; the caller reads the
/// archive again.
fn lost<T: SignalTransport>(shared: &Shared<T>) -> Result<Delivered> {
    let now = shared.now();
    shared.db.zone(|tx| forget(tx, now))?;
    Ok(Delivered::StageLost)
}

/// The delivery rows of the stage in `dir`, in the order a delivery appends them, with no
/// account behind them: no row of `backend.db` is written, a chat's features are not put
/// in front, nothing is dropped for being due. For tests that feed a core directly
/// (`archive_testing::delivery_rows`).
#[cfg(feature = "testing")]
pub(crate) fn plain_rows(
    dir: &std::path::Path,
    key: &StageKey,
    bounds: DeliveryBounds,
) -> std::result::Result<Vec<Vec<BackendEvent>>, StageError> {
    let recorded = {
        let reader = StageReader::open(dir, key)?;
        let totals = reader.totals()?;
        Recorded {
            messages: totals.messages,
            chats: totals.chats,
            frames: reader.frames()?,
        }
    };
    let start = FeedStart {
        cursor: Cursor::START,
        redo: Vec::new(),
        delivered: 0,
    };
    // Everything at once: a test's stage is small.
    let (tx, mut rx) = mpsc::channel(1 << 20);
    feed(dir, key, recorded, &start, bounds, &tx);
    drop(tx);
    let mut rows = Vec::new();
    while let Ok(fed) = rx.try_recv() {
        let chunk = match fed {
            Fed::Chunk(chunk) => *chunk,
            Fed::End { whole: true } => break,
            Fed::End { whole: false } => return Err(StageError::Unreadable),
            Fed::Failed(e) => return Err(e),
        };
        let events = match chunk.payload {
            Payload::Contacts(contacts) => contacts
                .iter()
                .map(|contact| BackendEvent::ContactUpsert(contact_patch(contact, false)))
                .collect(),
            Payload::Chats(chats) => chats
                .iter()
                .map(|(chat, blocked)| {
                    // A group's own flag (`group_event` reads it in a real delivery).
                    let group_blocked = chat.op.group().is_some_and(|group| group.blocked);
                    chat_event(chat, *blocked || group_blocked)
                })
                .collect(),
            Payload::Messages {
                conv,
                messages,
                has_more,
                progress,
                ..
            } => {
                let mut events = Vec::with_capacity(1);
                if !messages.is_empty() {
                    events.push(BackendEvent::HistoryBatch {
                        conv,
                        messages,
                        has_more,
                        progress,
                    });
                }
                events
            }
            Payload::End { .. } => Vec::new(),
            Payload::Pins(keys) => vec![BackendEvent::PinnedConversations(keys)],
        };
        if !events.is_empty() {
            rows.push(events);
        }
    }
    Ok(rows)
}

#[cfg(test)]
mod tests;
