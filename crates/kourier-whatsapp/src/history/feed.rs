//! The feeder, first pass (W4 C11): reading a captured chunk once to PLAN its delivery.
//!
//! A chunk is a zlib `HistorySync`: its chats come one after the other and everything else (push names,
//! the pairs of ids, the chunk's own order and progress, the sync type) only AT THE END, as the
//! remainder (`HistorySyncStream::remainder`). The delivery wants the opposite order: the people first,
//! then the chats, then the messages of the most recent chat first. So a chunk is read twice: this pass
//! reads it whole once, a chat at a time, and writes what the delivery needs to find its way before it
//! reads the chunk again: for every chat its place in the stream, the id it is placed by, the time of its
//! last message and how many messages it holds, in the ORDER OF DELIVERY (most recent first; equal times
//! by the id, then by the stream position, so the order is a function of the bytes and nothing else);
//! and the chunk's sync type, order and progress. The chunk's bytes are the same on every pass, so a
//! kill between the passes finds the same plan, and a plan that was not committed is made again.
//!
//! ALL IN ONE STAGE JOB: the plan rows and the chunk's state `planned` commit together or not at all, and
//! only a chunk still `captured` is planned (a second planning of a chunk changes nothing).
//!
//! A chunk that does not read (not a zlib stream, inflating past [`MAX_DECOMPRESSED`], a truncated or
//! malformed protobuf) is marked `bad`: kept 14 days for a later build, never delivered, returned to the
//! caller to count. One chat of a chunk that does not decode does not spoil the others: it is skipped and
//! counted in `chunks.skipped` (what upstream's own reader does with it).
//!
//! BLOCKING: inflating 5-20 MB takes 50-500 ms. [`plan`] and [`plan_pending`] run on the feeder's thread,
//! never on the actor's runtime. Nothing here logs an id or a byte; a log line names a kind and a count.

use kourier_db::Priority;
use kourier_db::rusqlite::{Connection, OptionalExtension as _, params};
use wacore::history_sync::{HistorySyncError, HistorySyncStream, MAX_DECOMPRESSED};
use wacore::store::error::StoreError;
use waproto::whatsapp as wa;

use super::map::Place;
use super::stage::StageHandle;
use crate::error::{self, sql};

/// Why a chunk did not read. The word is what the log line says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Bad {
    /// The stage holds no bytes for it.
    Missing,
    /// The bytes are no zlib stream, or inflate past the bound.
    Inflate,
    /// The protobuf is truncated or malformed.
    Decode,
}

impl Bad {
    pub(crate) fn of(error: &HistorySyncError) -> Self {
        match error {
            HistorySyncError::DecompressionError(_) => Self::Inflate,
            _ => Self::Decode,
        }
    }

    /// The word for a log line.
    pub(crate) fn word(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Inflate => "inflate",
            Self::Decode => "decode",
        }
    }
}

/// One chat of a chunk, as the plan holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Entry {
    /// The chat's place in the order of delivery (0 is the most recent chat): the cursor's `conv`.
    pub(crate) n: u32,
    /// The chat's place in the chunk's stream (counting an entry that did not decode): where the
    /// delivery finds it again.
    pub(crate) pos: u32,
    /// The id the chat was placed by ([`Place::jid`]).
    pub(crate) conv: String,
    /// The time of the chat's last message in seconds (0: the chat states none).
    pub(crate) ts: i64,
    /// How many messages the chat holds in the chunk: every entry of its list, shown or not.
    pub(crate) count: u32,
}

/// What reading a chunk once gave.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Scan {
    /// The chats in the order of delivery.
    pub(crate) entries: Vec<Entry>,
    /// The chunk's own sync type (`HistorySyncType` number).
    pub(crate) sync_type: i32,
    /// `HistorySync.chunkOrder`.
    pub(crate) order: Option<u32>,
    /// `HistorySync.progress`, 0 to 100.
    pub(crate) progress: Option<u32>,
    /// Chats of the chunk that did not decode.
    pub(crate) skipped: u32,
}

/// Reads the whole chunk `bytes` once. Pure: no stage, no clock.
pub(crate) fn scan(bytes: &[u8], limit: u64) -> Result<Scan, Bad> {
    let mut stream = HistorySyncStream::new(bytes, limit);
    let mut conversation = wa::Conversation::default();
    let mut found: Vec<Entry> = Vec::new();
    let mut skipped = 0u32;
    let mut pos = 0u32;
    while let Some(raw) = stream.next_conversation_bytes().map_err(|e| Bad::of(&e))? {
        let here = pos;
        pos = pos.saturating_add(1);
        if waproto::codec::conversation_merge_from_slice(&mut conversation, raw).is_err() {
            skipped = skipped.saturating_add(1);
            continue;
        }
        found.push(Entry {
            n: 0,
            pos: here,
            conv: Place::of(&conversation).jid,
            ts: time_of(&conversation),
            count: u32::try_from(conversation.messages.len()).unwrap_or(u32::MAX),
        });
    }
    let rest = stream.remainder().map_err(|e| Bad::of(&e))?;
    // Most recent first; the id and then the position settle equal times, so the order is the same on
    // every pass over the same bytes.
    found.sort_by(|a, b| {
        b.ts.cmp(&a.ts)
            .then_with(|| a.conv.cmp(&b.conv))
            .then_with(|| a.pos.cmp(&b.pos))
    });
    for (n, entry) in (0u32..).zip(found.iter_mut()) {
        entry.n = n;
    }
    Ok(Scan {
        entries: found,
        sync_type: rest.sync_type as i32,
        order: rest.chunk_order,
        progress: rest.progress,
        skipped,
    })
}

/// The time (seconds) a chat is ordered by: the chat's own activity time, else its last message's, else
/// the newest message in the chunk, else 0.
fn time_of(conversation: &wa::Conversation) -> i64 {
    let newest = || {
        conversation
            .messages
            .iter()
            .filter_map(|entry| entry.message.as_option())
            .filter_map(|info| info.message_timestamp)
            .max()
    };
    conversation
        .conversation_timestamp
        .filter(|seconds| *seconds > 0)
        .or(conversation
            .last_msg_timestamp
            .filter(|seconds| *seconds > 0))
        .or_else(newest)
        .and_then(|seconds| i64::try_from(seconds).ok())
        .unwrap_or(0)
}

/// What planning one chunk did.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Planned {
    /// The chunk read: `chats` entries are in its plan, `skipped` chats did not decode.
    Read { chats: u32, skipped: u32 },
    /// The chunk did not read and is `bad` now: the caller counts it.
    Bad(Bad),
    /// Nothing to do: the chunk is not (or no longer) `captured`.
    Nothing,
}

/// Plans chunk `seq` if it is still `captured`: reads it once and commits the plan and the state
/// together. See the module docs.
pub(crate) fn plan(stage: &StageHandle, seq: i64) -> Result<Planned, StoreError> {
    plan_with(stage, seq, MAX_DECOMPRESSED)
}

/// [`plan`] with another bound on the inflated size (a test makes a small one).
pub(crate) fn plan_with(stage: &StageHandle, seq: i64, limit: u64) -> Result<Planned, StoreError> {
    let bytes = {
        let conn = stage
            .reader()
            .map_err(|_| error::database("the history stage could not be read"))?;
        // `None`: not captured (or not there). `Some(None)`: captured and without bytes.
        conn.prepare_cached(
            "SELECT b.bytes FROM chunks c LEFT JOIN chunk_bytes b ON b.seq = c.seq \
             WHERE c.seq = ?1 AND c.state = 'captured'",
        )
        .and_then(|mut s| {
            s.query_row([seq], |r| r.get::<_, Option<Vec<u8>>>(0))
                .optional()
        })
        .map_err(sql("read a captured history chunk"))?
    };
    let Some(bytes) = bytes else {
        return Ok(Planned::Nothing);
    };
    let outcome = match bytes {
        Some(bytes) => scan(&bytes, limit),
        None => Err(Bad::Missing),
    };
    match outcome {
        Ok(scan) => commit_plan(stage, seq, scan),
        Err(bad) => mark_bad(stage, seq, bad),
    }
}

fn commit_plan(stage: &StageHandle, seq: i64, scan: Scan) -> Result<Planned, StoreError> {
    let (chats, skipped) = (
        u32::try_from(scan.entries.len()).unwrap_or(u32::MAX),
        scan.skipped,
    );
    let changed = stage.write_blocking(Priority::Low, "plan a history chunk", None, move |tx| {
        let changed = tx
            .prepare_cached(
                "UPDATE chunks SET state = 'planned', sync_type = ?2, chunk_order = ?3, \
                 progress = ?4, skipped = ?5 WHERE seq = ?1 AND state = 'captured'",
            )
            .and_then(|mut s| {
                s.execute(params![
                    seq,
                    scan.sync_type,
                    scan.order,
                    scan.progress,
                    scan.skipped
                ])
            })
            .map_err(sql("plan a history chunk"))?;
        if changed == 0 {
            // Another pass planned it first: its plan stands.
            return Ok(false);
        }
        let mut insert = tx
            .prepare_cached(
                "INSERT INTO plan (chunk, n, pos, conv, ts, count) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )
            .map_err(sql("write a history plan"))?;
        for entry in &scan.entries {
            insert
                .execute(params![
                    seq,
                    entry.n,
                    entry.pos,
                    entry.conv,
                    entry.ts,
                    entry.count
                ])
                .map_err(sql("write a history plan"))?;
        }
        Ok(true)
    })?;
    if !changed {
        return Ok(Planned::Nothing);
    }
    if skipped > 0 {
        tracing::warn!(
            skipped,
            "chats of a history chunk did not decode and are skipped"
        );
    }
    Ok(Planned::Read { chats, skipped })
}

fn mark_bad(stage: &StageHandle, seq: i64, bad: Bad) -> Result<Planned, StoreError> {
    let changed =
        stage.write_blocking(Priority::Low, "mark a history chunk bad", None, move |tx| {
            tx.prepare_cached(
                "UPDATE chunks SET state = 'bad', bad_at = ?2 WHERE seq = ?1 AND state = 'captured'",
            )
            .and_then(|mut s| s.execute((seq, kourier_db::now_ms())))
            .map_err(sql("mark a history chunk bad"))
        })?;
    if changed == 0 {
        return Ok(Planned::Nothing);
    }
    tracing::warn!(
        kind = bad.word(),
        "a history chunk did not read and is kept aside"
    );
    Ok(Planned::Bad(bad))
}

/// What [`plan_pending`] did.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Report {
    /// Chunks planned.
    pub(crate) planned: u32,
    /// Chunks that did not read.
    pub(crate) bad: u32,
}

/// Plans every `captured` chunk, the oldest capture first. The driver calls it when a chunk is
/// captured and at the start; a chunk captured meanwhile is taken by the next call.
pub(crate) fn plan_pending(stage: &StageHandle) -> Result<Report, StoreError> {
    let mut report = Report::default();
    let mut after = 0i64;
    loop {
        let next = {
            let conn = stage
                .reader()
                .map_err(|_| error::database("the history stage could not be read"))?;
            conn.prepare_cached(
                "SELECT seq FROM chunks WHERE state = 'captured' AND seq > ?1 ORDER BY seq LIMIT 1",
            )
            .and_then(|mut s| s.query_row([after], |r| r.get::<_, i64>(0)).optional())
            .map_err(sql("find a captured history chunk"))?
        };
        let Some(seq) = next else {
            return Ok(report);
        };
        after = seq;
        match plan(stage, seq)? {
            Planned::Read { .. } => report.planned += 1,
            Planned::Bad(_) => report.bad += 1,
            Planned::Nothing => {}
        }
    }
}

/// The plan of chunk `seq` in the order of delivery.
pub(crate) fn entries(conn: &Connection, seq: i64) -> Result<Vec<Entry>, StoreError> {
    let mut statement = conn
        .prepare_cached("SELECT n, pos, conv, ts, count FROM plan WHERE chunk = ?1 ORDER BY n")
        .map_err(sql("read a history plan"))?;
    let rows = statement
        .query_map([seq], |r| {
            Ok(Entry {
                n: r.get(0)?,
                pos: r.get(1)?,
                conv: r.get(2)?,
                ts: r.get(3)?,
                count: r.get(4)?,
            })
        })
        .map_err(sql("read a history plan"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(sql("read a history plan"))
}

#[cfg(test)]
mod tests;
