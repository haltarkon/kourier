//! The feeder, second pass (W4 C12): the ROWS of a planned chunk, from wherever the cursor stands.
//!
//! The first pass ([`super::feed`]) planned the chunk: its chats in the order of delivery. This pass reads
//! the chunk again and cuts it into the rows the delivery zone appends, one after the other, in the order
//! the core wants them:
//! 1. PEOPLE (phase 0): the chunk's pairs and push names ([`super::rest::people`]), at most
//!    [`Bounds::people`] events a row, pairs first. The parts that are no message ([`super::rest::kept`])
//!    ride on the FIRST row of the phase, so they are written exactly once with it.
//! 2. CHATS (phase 1): the chats' entries, the LEAST recent first (so the most recent chat ends up on top
//!    of the list), at most [`Bounds::chats`] chats a row. A row carries the [`Chat`] objects, not
//!    events: the zone states the unread hint, which depends on what it has appended meanwhile.
//! 3. MESSAGES (phase 2): the chats in the order of delivery (the MOST recent first), each chat's messages
//!    OLDEST first, at most [`Bounds::messages`] entries and about [`Bounds::bytes`] a row.
//!
//! THE CURSOR is a position in this order and the ONLY state of the walk: [`Reader::next`] is a function of
//! the chunk's bytes, its plan and the cursor it is given, so a row read again from the same cursor is the
//! same row (and the rows after a kill are the rows that would have followed). A row's `before` is the
//! cursor it was asked for, exactly: the zone refuses a row that does not begin where the stored cursor
//! stands. Its `after` is the position the zone stores with it. A position with nothing in it (a chat with
//! no messages, a status chat, a phase with no rows) is walked over by the plan alone, without reading the
//! chunk, so a cursor may stand on one: the next call goes past it.
//!
//! WHERE THE NUMBERS COUNT. Phase 0 `msg`: events appended. Phase 1 `msg`: plan entries consumed from the
//! least recent end (`n = total - 1 - msg`), those that give no chat included. Phase 2 `conv`: the plan's
//! `n` of the chat; `msg`: entries of the chat's list consumed, in the SORTED list (oldest first; equal
//! seconds in the reverse of the phone's order, which lists the newest first), whether they gave a row or
//! not. Phase 3 ([`Phase::End`]) is the end of the chunk's content: the end rows ([`Body::End`]) live there.
//!
//! A messages row ([`Body::Messages`]) holds the [`Piece`]s the zone must act on: a message for the core, or
//! a part this build keeps (`history_deferred`, with the number [`deferred_n`]). What is let go (a status
//! update, a key share) is consumed and not carried, so a row may hold no piece at all; the cursor still
//! moves. A message larger than the byte bound travels alone.
//!
//! The row's byte bound counts the ENCODED size of the phone's entries, which is what is staged, not the
//! mapped events.
//!
//! BLOCKING: inflating the chunk takes 50-500 ms and a chat of tens of thousands of messages is decoded in
//! one go; the reader belongs on the feeder's thread, never on the actor's runtime. One chat is held in
//! memory at a time. Nothing here logs an id or a byte.

use std::collections::HashMap;

use kourier_backend_api::{BackendEvent, MAX_HISTORY_BATCH};
use kourier_db::rusqlite::OptionalExtension as _;
use kourier_model::ConvKey;
use wacore::history_sync::{HistorySyncStream, MAX_DECOMPRESSED};
use wacore::store::error::StoreError;
use waproto::buffa::Message as _;
use waproto::whatsapp as wa;

use super::chat::{self, Chat};
use super::feed::{Bad, Entry, entries};
use super::map::{self, Item, Place, Target};
use super::rest::{self, Part};
use super::stage::StageHandle;
use crate::error::{self, sql};
use crate::map::Ctx;

/// The staged bytes one row holds at most (Signal's bound: the core commits a row in one transaction and a
/// megabyte keeps it small). A message larger than this travels alone.
pub(crate) const MAX_ROW_BYTES: usize = 1024 * 1024;

/// Chats in one chats row.
pub(crate) const CHATS_PER_ROW: usize = 50;

/// The `history_deferred.n` of the kept parts that are no message start here, far above every
/// [`deferred_n`].
const PART_BASE: i64 = 1 << 62;

/// The `history_deferred.n` of the message at `index` of the SORTED list of the chat at plan place `conv`:
/// a function of the chunk and the place, so a row read again numbers its items the same.
pub(crate) fn deferred_n(conv: u32, index: u32) -> i64 {
    (i64::from(conv) << 32) | i64::from(index)
}

/// The `history_deferred.n` of the `i`-th kept part of a chunk.
pub(crate) fn part_n(i: u32) -> i64 {
    PART_BASE + i64::from(i)
}

/// Where in the order of a chunk's rows a position stands (the cursor's `phase`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Phase {
    /// The chunk's pairs and push names.
    People = 0,
    /// The chats' entries.
    Chats = 1,
    /// The messages of the chats.
    Messages = 2,
    /// Past the content of the chunk: only its end rows are left.
    End = 3,
}

impl Phase {
    /// The number stored in `history_cursor.phase`.
    pub(crate) fn number(self) -> i64 {
        self as i64
    }

    /// The phase a stored number names.
    pub(crate) fn of_number(number: i64) -> Option<Self> {
        match number {
            0 => Some(Self::People),
            1 => Some(Self::Chats),
            2 => Some(Self::Messages),
            3 => Some(Self::End),
            _ => None,
        }
    }
}

/// A position in the rows of the stage: `history_cursor`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Cursor {
    /// The stage's `chunks.seq`.
    pub(crate) chunk: i64,
    pub(crate) phase: Phase,
    /// The plan place `n` of the chat being filled (phase 2).
    pub(crate) conv: u32,
    /// What is appended of the phase (or of the chat): see the module docs.
    pub(crate) msg: u32,
}

impl Cursor {
    /// The first position of a chunk.
    pub(crate) fn start(chunk: i64) -> Self {
        Self::at(chunk, Phase::People, 0, 0)
    }

    pub(crate) fn at(chunk: i64, phase: Phase, conv: u32, msg: u32) -> Self {
        Self {
            chunk,
            phase,
            conv,
            msg,
        }
    }
}

/// The bounds of a row. The defaults are what the delivery uses; the take-back of a refused row reads it
/// again with [`Bounds::SINGLE`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Bounds {
    /// Events of the people phase in a row.
    pub(crate) people: usize,
    /// Chats in a chats row.
    pub(crate) chats: usize,
    /// Entries of a chat's list in a messages row (the core refuses more than 500 in a batch).
    pub(crate) messages: usize,
    /// Staged bytes in a messages row.
    pub(crate) bytes: usize,
}

impl Bounds {
    pub(crate) const DEFAULT: Self = Self {
        people: rest::MAX_PEOPLE_PER_ROW,
        chats: CHATS_PER_ROW,
        messages: MAX_HISTORY_BATCH,
        bytes: MAX_ROW_BYTES,
    };

    /// One unit a row: one event, one chat, one message.
    pub(crate) const SINGLE: Self = Self {
        people: 1,
        chats: 1,
        messages: 1,
        bytes: usize::MAX,
    };
}

/// A kept part that is no message, with the number it is stored under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Kept {
    /// `history_deferred.n`.
    pub(crate) n: i64,
    pub(crate) part: Part,
}

/// One entry of a chat's list that the zone must act on.
#[derive(Debug)]
pub(crate) struct Piece {
    /// The entry's place in the chat's SORTED list ([`deferred_n`] numbers it).
    pub(crate) index: u32,
    /// The phone's entry, as it stands in the chunk: the zone reads the message secret off it and, for a
    /// deferred item, encodes it as it is.
    pub(crate) info: wa::WebMessageInfo,
    /// What the map made of it: a message for the core, or a part kept for a later build
    /// (never [`Item::Skip`]).
    pub(crate) item: Item,
}

/// What a row carries.
#[derive(Debug)]
pub(crate) enum Body {
    /// Events (pairs, then names) and, on the phase's first row, the kept parts that are no message. It may
    /// hold no events at all (a chunk of only kept parts): the zone then appends nothing to `core_outbox`.
    People {
        events: Vec<BackendEvent>,
        parts: Vec<Kept>,
    },
    /// Chats, the least recent first.
    Chats(Vec<Chat>),
    /// Entries of one chat's list, oldest first. `place` is the chat; a chat this build has no conversation
    /// for ([`Target::Unmapped`]) holds nothing but deferred pieces.
    Messages {
        /// The chat's plan place.
        conv: u32,
        place: Place,
        /// The chunk's own `Conversation.id` of the chat: what upstream keys the messages' secrets by
        /// (`place.jid` is the form the chat was placed by, which may be its LID where this is its number).
        chat_id: String,
        pieces: Vec<Piece>,
    },
    /// The end of a chunk's content, and the end rows: `finished` is set on the row that moves the cursor past
    /// the chunk's content (the zone records the chunk as delivered), `convs` are the conversations whose
    /// push is over (one empty batch each, `has_more: false`). Either may be empty. Such a row is never read
    /// by [`Reader::next`]: the feeder makes it ([`super::feeder`]).
    End {
        finished: Option<ChunkEnd>,
        convs: Vec<ConvKey>,
    },
}

/// What the end of a push needs to know of a chunk whose content is all appended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ChunkEnd {
    pub(crate) sync_type: Option<i32>,
    pub(crate) order: Option<u32>,
    /// The phone's own progress of the chunk, 0 to 100.
    pub(crate) progress: Option<u32>,
    /// Chats of the chunk that did not decode (the first pass counted them): folded into the counts.
    pub(crate) skipped: u32,
}

/// One row: what to append, and the positions around it.
#[derive(Debug)]
pub(crate) struct Row {
    /// The cursor the row was read from: where the stored cursor must stand for the zone to append it.
    pub(crate) before: Cursor,
    /// Where the cursor stands after it.
    pub(crate) after: Cursor,
    pub(crate) body: Body,
}

/// What [`Reader::next`] found.
#[derive(Debug)]
pub(crate) enum Next {
    Row(Row),
    /// Nothing is left of the chunk's content from this cursor on.
    End,
}

/// A chunk the first pass planned, as the second pass needs it.
#[derive(Debug)]
pub(crate) struct Loaded {
    /// The stage's key of the chunk (the id of the notice that announced it).
    pub(crate) msg_id: String,
    /// The zlib `HistorySync` as the phone uploaded it.
    pub(crate) bytes: Vec<u8>,
    /// The plan, in the order of delivery.
    pub(crate) entries: Vec<Entry>,
    pub(crate) sync_type: Option<i32>,
    pub(crate) order: Option<u32>,
    pub(crate) progress: Option<u32>,
    /// Chats of the chunk that did not decode (the first pass counted them).
    pub(crate) skipped: u32,
}

/// Chunk `seq` as the stage holds it, if it is `planned` and still has its bytes.
pub(crate) fn load(stage: &StageHandle, seq: i64) -> Result<Option<Loaded>, StoreError> {
    let conn = stage
        .reader()
        .map_err(|_| error::database("the history stage could not be read"))?;
    let head = conn
        .prepare_cached(
            "SELECT b.bytes, c.sync_type, c.chunk_order, c.progress, c.skipped, c.msg_id \
             FROM chunks c LEFT JOIN chunk_bytes b ON b.seq = c.seq \
             WHERE c.seq = ?1 AND c.state = 'planned'",
        )
        .and_then(|mut s| {
            s.query_row([seq], |r| {
                Ok((
                    r.get::<_, Option<Vec<u8>>>(0)?,
                    r.get::<_, Option<i32>>(1)?,
                    r.get::<_, Option<u32>>(2)?,
                    r.get::<_, Option<u32>>(3)?,
                    r.get::<_, u32>(4)?,
                    r.get::<_, String>(5)?,
                ))
            })
            .optional()
        })
        .map_err(sql("read a planned history chunk"))?;
    let Some((Some(bytes), sync_type, order, progress, skipped, msg_id)) = head else {
        return Ok(None);
    };
    Ok(Some(Loaded {
        msg_id,
        bytes,
        entries: entries(&conn, seq)?,
        sync_type,
        order,
        progress,
        skipped,
    }))
}

/// The chunk read for its people and its chats: one walk of the stream, kept for the phases that need it.
struct Prepared {
    people: Vec<BackendEvent>,
    parts: Vec<Part>,
    /// The chat of each plan place (`None`: it gives no chat entry).
    chats: Vec<Option<Chat>>,
}

/// The chat being filled: its list in the sorted order.
struct Current {
    conv: u32,
    /// The chunk's `Conversation.id` of the chat.
    chat_id: String,
    /// `None`: an entry with no message in it.
    sorted: Vec<Option<wa::WebMessageInfo>>,
}

/// A stream of the chunk and how many chats it has yielded: chats come in the order of delivery, which is
/// mostly the order of the stream, so one walk usually serves them all.
struct Seeker<'a> {
    stream: HistorySyncStream<'a>,
    next: u32,
}

/// Reads the rows of ONE planned chunk. See the module docs.
pub(crate) struct Reader<'a> {
    bytes: &'a [u8],
    limit: u64,
    entries: &'a [Entry],
    ctx: &'a Ctx,
    now_ms: i64,
    prepared: Option<Prepared>,
    current: Option<Current>,
    seeker: Option<Seeker<'a>>,
}

impl<'a> Reader<'a> {
    /// A reader over the chunk `bytes` with its `entries` (the plan). `now_ms` is the local clock the rows'
    /// `received_at` is made of.
    pub(crate) fn new(bytes: &'a [u8], entries: &'a [Entry], ctx: &'a Ctx, now_ms: i64) -> Self {
        Self::with_limit(bytes, entries, ctx, now_ms, MAX_DECOMPRESSED)
    }

    /// [`new`](Self::new) with another bound on the inflated size.
    pub(crate) fn with_limit(
        bytes: &'a [u8],
        entries: &'a [Entry],
        ctx: &'a Ctx,
        now_ms: i64,
        limit: u64,
    ) -> Self {
        Self {
            bytes,
            limit,
            entries,
            ctx,
            now_ms,
            prepared: None,
            current: None,
            seeker: None,
        }
    }

    /// The row that begins at `at` (or the first row after a position with nothing in it), or
    /// [`Next::End`] when none is left. `Err`: the chunk does not read any more (it did when it was
    /// planned; the caller treats it as a bad chunk).
    pub(crate) fn next(&mut self, at: Cursor, bounds: Bounds) -> Result<Next, Bad> {
        let mut from = at;
        loop {
            let found = match from.phase {
                Phase::People => self.people_row(from, bounds)?,
                Phase::Chats => self.chats_row(from, bounds)?,
                Phase::Messages => self.messages_row(from, bounds)?,
                Phase::End => return Ok(Next::End),
            };
            match found {
                Some((body, after)) => {
                    return Ok(Next::Row(Row {
                        before: at,
                        after,
                        body,
                    }));
                }
                None => from = self.after_phase(from),
            }
        }
    }

    /// The first position of the phase that follows the one `from` stands in.
    fn after_phase(&self, from: Cursor) -> Cursor {
        match from.phase {
            Phase::People => Cursor::at(from.chunk, Phase::Chats, 0, 0),
            Phase::Chats => self.messages_from(from.chunk, 0, 0),
            Phase::Messages | Phase::End => Cursor::at(from.chunk, Phase::End, 0, 0),
        }
    }

    /// The people phase has one position for each event of the people and then one for each kept part (W4 fix
    /// LOW-4), so a chunk with a great deal of kept material (call records, former members, the phone's address
    /// book) is cut into rows like any other: a row holds at most `bounds.people` units (events and parts) and, once
    /// it holds something, no part that would take it past `bounds.bytes`. A part is never split, and the first unit of
    /// a row always goes in, so the walk always moves.
    fn people_row(&mut self, from: Cursor, bounds: Bounds) -> Result<Option<(Body, Cursor)>, Bad> {
        let prepared = self.prepared()?;
        let total = prepared.people.len();
        let positions = total.saturating_add(prepared.parts.len());
        let start = usize::try_from(from.msg).unwrap_or(usize::MAX);
        if start >= positions {
            return Ok(None);
        }
        let room = bounds.people.max(1);
        let events_end = if start < total {
            start.saturating_add(room).min(total)
        } else {
            start
        };
        let events = if start < total {
            prepared
                .people
                .get(start..events_end)
                .unwrap_or_default()
                .to_vec()
        } else {
            Vec::new()
        };
        let mut next = events_end;
        let mut parts = Vec::new();
        let mut bytes = 0usize;
        while next >= total && next < positions && events.len() + parts.len() < room {
            let index = next - total;
            let Some(part) = prepared.parts.get(index) else {
                break;
            };
            let size = part.item.len();
            let started = !events.is_empty() || !parts.is_empty();
            if started && bytes.saturating_add(size) > bounds.bytes {
                break;
            }
            bytes = bytes.saturating_add(size);
            parts.push(Kept {
                n: part_n(u32::try_from(index).unwrap_or(u32::MAX)),
                part: part.clone(),
            });
            next += 1;
        }
        let after = if next >= positions {
            Cursor::at(from.chunk, Phase::Chats, 0, 0)
        } else {
            Cursor::at(
                from.chunk,
                Phase::People,
                0,
                u32::try_from(next).unwrap_or(u32::MAX),
            )
        };
        Ok(Some((Body::People { events, parts }, after)))
    }

    fn chats_row(&mut self, from: Cursor, bounds: Bounds) -> Result<Option<(Body, Cursor)>, Bad> {
        let prepared = self.prepared()?;
        let total = prepared.chats.len();
        let mut consumed = usize::try_from(from.msg).unwrap_or(usize::MAX);
        let mut chats = Vec::new();
        while consumed < total && chats.len() < bounds.chats.max(1) {
            // The least recent entry first: the plan's last place.
            if let Some(chat) = prepared
                .chats
                .get(total - 1 - consumed)
                .and_then(Option::as_ref)
            {
                chats.push(chat.clone());
            }
            consumed += 1;
        }
        if chats.is_empty() {
            return Ok(None);
        }
        let after = if consumed >= total {
            self.messages_from(from.chunk, 0, 0)
        } else {
            Cursor::at(
                from.chunk,
                Phase::Chats,
                0,
                u32::try_from(consumed).unwrap_or(u32::MAX),
            )
        };
        Ok(Some((Body::Chats(chats), after)))
    }

    fn messages_row(
        &mut self,
        from: Cursor,
        bounds: Bounds,
    ) -> Result<Option<(Body, Cursor)>, Bad> {
        // Walk to a chat with something in it: by the plan alone.
        let landed = self.messages_from(from.chunk, from.conv, from.msg);
        if landed.phase != Phase::Messages {
            return Ok(None);
        }
        let (conv, start) = (
            landed.conv,
            usize::try_from(landed.msg).unwrap_or(usize::MAX),
        );
        let Some(place) = self
            .entries
            .get(conv as usize)
            .map(|entry| Place::from_jid(&entry.conv))
        else {
            return Ok(None);
        };
        self.load_chat(conv)?;
        let (now_ms, ctx) = (self.now_ms, self.ctx);
        let Some(current) = self.current.as_ref() else {
            return Ok(None);
        };
        let len = current.sorted.len();
        let mut pieces = Vec::new();
        let (mut bytes, mut taken) = (0usize, 0usize);
        while start + taken < len && taken < bounds.messages.max(1) {
            let slot = current.sorted.get(start + taken).and_then(Option::as_ref);
            let size = slot.map_or(0, |info| info.encoded_len() as usize);
            // A message larger than the bound travels alone, but never as the second of a row.
            if taken > 0 && bytes.saturating_add(size) > bounds.bytes {
                break;
            }
            bytes = bytes.saturating_add(size);
            if let Some(info) = slot {
                match map::message(&place, info, ctx, now_ms) {
                    Item::Skip => {}
                    item => pieces.push(Piece {
                        index: u32::try_from(start + taken).unwrap_or(u32::MAX),
                        info: info.clone(),
                        item,
                    }),
                }
            }
            taken += 1;
        }
        let after = self.messages_from(
            from.chunk,
            conv,
            u32::try_from(start + taken).unwrap_or(u32::MAX),
        );
        Ok(Some((
            Body::Messages {
                conv,
                place,
                chat_id: current.chat_id.clone(),
                pieces,
            },
            after,
        )))
    }

    /// The first position at or after (`conv`, `msg`) that has messages to read, by the plan alone: a chat
    /// with none, one whose list is read to its end and a status chat are walked over. [`Phase::End`] when
    /// there is none.
    fn messages_from(&self, chunk: i64, mut conv: u32, mut msg: u32) -> Cursor {
        while let Some(entry) = self.entries.get(conv as usize) {
            let status = Place::from_jid(&entry.conv).target == Target::Status;
            if !status && msg < entry.count {
                return Cursor::at(chunk, Phase::Messages, conv, msg);
            }
            conv = conv.saturating_add(1);
            msg = 0;
        }
        Cursor::at(chunk, Phase::End, 0, 0)
    }

    fn prepared(&mut self) -> Result<&Prepared, Bad> {
        let prepared = match self.prepared.take() {
            Some(prepared) => prepared,
            None => prepare(self.bytes, self.limit, self.entries, self.ctx)?,
        };
        Ok(self.prepared.insert(prepared))
    }

    /// Makes the chat at plan place `conv` the current one: read from the chunk, its list sorted.
    fn load_chat(&mut self, conv: u32) -> Result<(), Bad> {
        if self.current.as_ref().is_some_and(|c| c.conv == conv) {
            return Ok(());
        }
        let pos = self
            .entries
            .get(conv as usize)
            .map(|entry| entry.pos)
            .ok_or(Bad::Decode)?;
        let mut conversation = self.conversation_at(pos)?;
        let chat_id = std::mem::take(&mut conversation.id);
        // Oldest first; equal seconds in the reverse of the phone's order (it lists the newest first).
        let mut order: Vec<(u64, usize)> = conversation
            .messages
            .iter()
            .enumerate()
            .map(|(i, entry)| {
                let at = entry
                    .message
                    .as_option()
                    .and_then(|info| info.message_timestamp);
                (at.unwrap_or(0), i)
            })
            .collect();
        order.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| b.1.cmp(&a.1)));
        let sorted = order
            .into_iter()
            .map(|(_, i)| {
                conversation
                    .messages
                    .get_mut(i)
                    .and_then(|m| m.message.take())
            })
            .collect();
        self.current = Some(Current {
            conv,
            chat_id,
            sorted,
        });
        Ok(())
    }

    /// The chat at stream position `pos` (the first pass counted every entry, one that did not decode too).
    fn conversation_at(&mut self, pos: u32) -> Result<wa::Conversation, Bad> {
        if self.seeker.as_ref().is_none_or(|seeker| seeker.next > pos) {
            self.seeker = Some(Seeker {
                stream: HistorySyncStream::new(self.bytes, self.limit),
                next: 0,
            });
        }
        let Some(seeker) = self.seeker.as_mut() else {
            return Err(Bad::Decode);
        };
        loop {
            let raw = seeker
                .stream
                .next_conversation_bytes()
                .map_err(|e| Bad::of(&e))?
                .ok_or(Bad::Decode)?;
            let here = seeker.next;
            seeker.next = here.saturating_add(1);
            if here == pos {
                let mut conversation = wa::Conversation::default();
                waproto::codec::conversation_merge_from_slice(&mut conversation, raw)
                    .map_err(|_| Bad::Decode)?;
                return Ok(conversation);
            }
        }
    }
}

/// One walk of the chunk: every chat that gives an entry, and the people and parts of the remainder.
fn prepare(bytes: &[u8], limit: u64, entries: &[Entry], ctx: &Ctx) -> Result<Prepared, Bad> {
    let place_of: HashMap<u32, usize> = entries
        .iter()
        .map(|entry| (entry.pos, entry.n as usize))
        .collect();
    let mut chats: Vec<Option<Chat>> = vec![None; entries.len()];
    let mut stream = HistorySyncStream::new(bytes, limit);
    let mut conversation = wa::Conversation::default();
    let mut pos = 0u32;
    while let Some(raw) = stream.next_conversation_bytes().map_err(|e| Bad::of(&e))? {
        let here = pos;
        pos = pos.saturating_add(1);
        // A chat the plan does not know did not decode in the first pass either.
        let Some(&n) = place_of.get(&here) else {
            continue;
        };
        if waproto::codec::conversation_merge_from_slice(&mut conversation, raw).is_err() {
            continue;
        }
        if let Some(slot) = chats.get_mut(n) {
            *slot = chat::chat(&Place::of(&conversation), &conversation, ctx);
        }
    }
    let rest = stream.remainder().map_err(|e| Bad::of(&e))?;
    Ok(Prepared {
        people: rest::people(&rest, ctx),
        parts: rest::kept(&rest),
        chats,
    })
}

#[cfg(test)]
mod tests;
