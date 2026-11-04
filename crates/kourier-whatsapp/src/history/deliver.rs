//! The delivery zone (W4, C13): ONE row of a chunk appended to `core_outbox`, exactly once whatever ends the
//! process.
//!
//! [`append`] runs on the backend.db writer at low priority (live work is served first) and does, in ONE
//! transaction with kill points around its commit:
//! 1. THE LOOK at the cursor: the row must begin where `history_cursor` stands (a new chunk begins at its
//!    start once the cursor finished the one before it, or when none was started); else nothing is written
//!    and the answer is [`Outcome::OutOfStep`]. This is what makes a row appended twice impossible: the first
//!    commit moved the cursor, the second sees it has moved.
//! 2. what the row holds: the message secrets of its messages ([`super::secrets`], so the secret is in the
//!    store exactly when the message is in the core), the parts this build cannot show (`history_deferred`),
//!    the states of the chats it names (`history_chats`: the unread hint is decided HERE, from what the
//!    store holds now, because the rows in flight have not written it yet).
//! 3. ONE `core_outbox` row for all its events, after the contract check (a unit that fails it is dropped
//!    and counted: a refused row would be quarantined by the forwarder for good).
//! 4. its `history_delivery` entry (what a take-back needs: the cursor it began at and the cursor it left),
//!    the counts, and the cursor moved to the row's `after`.
//!
//! A row that carries no event (every unit dropped, or a people row of only kept parts) appends nothing to
//! `core_outbox`, writes no entry and still moves the cursor.
//!
//! [`redo`] is the same zone for a row that was TAKEN BACK (refused by the core, or not answered within the
//! bounds of [`super::settle`]): it appends ONE unit of it, read again from the stage, without the look at
//! the cursor and without moving it (the cursor passed this row long ago); the look it makes is at the
//! taken-back entry, which advances in the same transaction.
//!
//! [`take_up`] is the same zone for the parts an EARLIER build of the map deferred (`history_deferred`, kind 0, a
//! lower [`MAP_VERSION`]): a build that can show them now delivers them as ONE silent history row of their chat
//! (`has_more: false`, read: they are the phone's old messages), in the transaction that removes them from
//! `history_deferred`, so each is delivered once whatever ends the process. It has no cursor and no entry: the
//! parts do not come from the stage, so a take-back could not read them again; a message the contract check
//! refuses is dropped and counted, as in a stage row.
//!
//! [`take_up_calls`] is the same zone for the call-log records the push kept (`history_deferred` kind 1, W6a1 D13): once the
//! push is over, each becomes a quiet call line in the chat of its call (the chat is added when the core has none), in the
//! transaction that marks the record [`TAKEN_UP`]; the record stays (a core `Rebuild` cannot fetch the history again).
//!
//! The caller wakes the forwarder when [`Appended::seq`] is set. The kill points are
//! [`crate::failpoints::INSIDE_HISTORY_ROW`] / [`AFTER_HISTORY_ROW`](crate::failpoints::AFTER_HISTORY_ROW).
//! Nothing here logs an id, a name or a body.

use std::collections::BTreeMap;

use kourier_backend_api::{BackendEvent, Envelope, IncomingMessage, encode_outbox_row};
use kourier_db::outbox;
use kourier_db::rusqlite::{Connection, OptionalExtension as _, Transaction, params};
use kourier_model::{ConvKey, Patch};
use wacore::store::error::StoreError;
use wacore_binary::Jid;
use waproto::buffa::Message as _;
use waproto::whatsapp as wa;

use super::chat::{self, Chat, HintState};
use super::map::{self, Item, Place, Target};
use super::rest::Kind;
use super::rows::{Body, Bounds, ChunkEnd, Cursor, Kept, Phase, Piece, Row, deferred_n};
use super::{live, named, progress, secrets, settle};
use crate::calls::log as call_log;
use crate::db::Points;
use crate::error::{self, sql};
use crate::failpoints::{AFTER_HISTORY_ROW, INSIDE_HISTORY_ROW};
use crate::fault;
use crate::map::{Ctx, MAP_VERSION, OwnIds};
use crate::store::{KourierStore, exec, secret};

const POINTS: Points = Points {
    inside: INSIDE_HISTORY_ROW,
    after: AFTER_HISTORY_ROW,
};

/// What the zone needs to know about the chunk a row came from.
#[derive(Debug, Clone)]
pub(crate) struct Source {
    /// The stage's key of the chunk (`chunks.msg_id`: the id of the notice that announced it).
    pub(crate) msg_id: String,
    /// The chunk's sync type, kept with the chats it touched.
    pub(crate) sync_type: Option<i32>,
    /// The push's progress (0.0 to 1.0) a messages row states.
    pub(crate) progress: Option<f32>,
}

/// Our account's own ids, for the secrets of a message of ours (it is keyed by both).
#[derive(Debug, Clone, Default)]
pub(crate) struct Own {
    pub(crate) pn: Option<Jid>,
    pub(crate) lid: Option<Jid>,
}

/// What an appended row became.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Appended {
    /// The row's place in `core_outbox`, when it had an event to append (wake the forwarder).
    pub(crate) seq: Option<i64>,
    /// Messages handed to the core in this row.
    pub(crate) delivered: u32,
    /// Messages the contract check refused (a mapping fault; they are not delivered).
    pub(crate) dropped: u32,
    /// Parts kept for a later build, new in this row.
    pub(crate) deferred: u32,
}

/// The answer of [`append`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    Appended(Appended),
    /// The row does not begin where the cursor stands: nothing was written.
    OutOfStep,
}

/// Appends `row` of the chunk `source` (module docs).
pub(crate) async fn append(
    store: &KourierStore,
    source: Source,
    own: Own,
    row: Row,
    now_ms: i64,
) -> Result<Outcome, StoreError> {
    store
        .sweep_at("append a history row", POINTS, move |tx| {
            zone(tx, &source, &own, row, now_ms)
        })
        .await
}

/// What one pass of [`take_up`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct TakenUp {
    /// The row's place in `core_outbox`, when something was delivered (wake the forwarder).
    pub(crate) seq: Option<i64>,
    /// Messages handed to the core.
    pub(crate) delivered: u32,
    /// Messages the contract check refused (not delivered, and not kept: they would fail again).
    pub(crate) dropped: u32,
    /// Items this build cannot show either: kept, now judged by this build's version.
    pub(crate) kept: u32,
    /// Items that turned out to be nothing to show (a status update, an empty entry): removed.
    pub(crate) skipped: u32,
    /// Items that do not read as a message any more (counted `bad`, kept for a later build).
    pub(crate) unread: u32,
}

/// A kept message of a lower map version, as read.
struct Due {
    msg_id: String,
    n: i64,
    bytes: Vec<u8>,
}

/// What this build makes of a [`Due`] item.
enum Verdict {
    Show(Box<IncomingMessage>, Box<wa::WebMessageInfo>),
    Keep,
    Skip,
    Unread,
}

/// Takes up ONE batch of the messages an earlier map deferred: the oldest kept message of a lower
/// [`MAP_VERSION`] and the others of its chat, at most [`Bounds::DEFAULT`]'s count and bytes. `None`: nothing is
/// due. Every item of the batch leaves the due set (delivered or skipped: removed; kept or unread: its version
/// is now this build's), so a caller that repeats the call until `None` ends.
pub(crate) async fn take_up(
    store: &KourierStore,
    own: Own,
    ctx: Ctx,
    now_ms: i64,
) -> Result<Option<TakenUp>, StoreError> {
    let Some((conv, due)) = store.read(due_batch).await? else {
        return Ok(None);
    };
    // Mapping is pure and happens outside the zone.
    let place = Place::from_jid(&conv);
    let verdicts: Vec<(String, i64, Verdict)> = due
        .into_iter()
        .map(|item| {
            let verdict = match wa::WebMessageInfo::decode_from_slice(&item.bytes) {
                Ok(info) => match map::message(&place, &info, &ctx, now_ms) {
                    Item::Message(message) => Verdict::Show(message, Box::new(info)),
                    Item::Deferred(_) => Verdict::Keep,
                    Item::Skip => Verdict::Skip,
                },
                Err(_) => Verdict::Unread,
            };
            (item.msg_id, item.n, verdict)
        })
        .collect();
    let target = place.target;
    store
        .sweep_at("take up deferred history", POINTS, move |tx| {
            take_up_zone(tx, &own, (&conv, &target), verdicts, now_ms)
        })
        .await
        .map(Some)
}

/// The oldest due item's chat and its due items (module docs).
fn due_batch(conn: &Connection) -> Result<Option<(String, Vec<Due>)>, StoreError> {
    let Some(conv) = conn
        .query_row(
            "SELECT conv FROM history_deferred WHERE kind = 0 AND map_version < ?1 \
             ORDER BY chunk_msg_id, n LIMIT 1",
            [MAP_VERSION],
            |r| r.get::<_, String>(0),
        )
        .optional()
        .map_err(sql("look for deferred history"))?
    else {
        return Ok(None);
    };
    let mut stmt = conn
        .prepare_cached(
            "SELECT chunk_msg_id, n, item FROM history_deferred \
             WHERE kind = 0 AND map_version < ?1 AND conv = ?2 ORDER BY chunk_msg_id, n LIMIT ?3",
        )
        .map_err(sql("read deferred history"))?;
    let limits = Bounds::DEFAULT;
    let rows = stmt
        .query_map(
            params![
                MAP_VERSION,
                conv,
                i64::try_from(limits.messages).unwrap_or(i64::MAX)
            ],
            |r| {
                Ok(Due {
                    msg_id: r.get(0)?,
                    n: r.get(1)?,
                    bytes: r.get(2)?,
                })
            },
        )
        .map_err(sql("read deferred history"))?;
    let (mut due, mut bytes) = (Vec::new(), 0usize);
    for row in rows {
        let row = row.map_err(sql("read deferred history"))?;
        // The row's size is bounded like a stage row's; one item is always taken.
        if !due.is_empty() && bytes.saturating_add(row.bytes.len()) > limits.bytes {
            break;
        }
        bytes += row.bytes.len();
        due.push(row);
    }
    Ok(Some((conv, due)))
}

/// The zone of [`take_up`]: the items leave `history_deferred` (or take this build's version), the messages go
/// into ONE batch with their secrets, the counts move, and the batch is a `core_outbox` row.
fn take_up_zone(
    tx: &Transaction<'_>,
    own: &Own,
    (conv_text, target): (&str, &Target),
    verdicts: Vec<(String, i64, Verdict)>,
    now: i64,
) -> Result<TakenUp, StoreError> {
    let mut done = TakenUp::default();
    let mut messages: Vec<IncomingMessage> = Vec::new();
    let mut infos: Vec<wa::WebMessageInfo> = Vec::new();
    for (msg_id, n, verdict) in verdicts {
        // The item must still be due: one made current meanwhile is not ours to deliver.
        let changed = match verdict {
            Verdict::Show(..) | Verdict::Skip => exec(
                tx,
                "take up a deferred history item",
                "DELETE FROM history_deferred \
                 WHERE chunk_msg_id = ?1 AND n = ?2 AND kind = 0 AND map_version < ?3",
                params![msg_id, n, MAP_VERSION],
            )?,
            Verdict::Keep | Verdict::Unread => exec(
                tx,
                "judge a deferred history item again",
                "UPDATE history_deferred SET map_version = ?3 \
                 WHERE chunk_msg_id = ?1 AND n = ?2 AND kind = 0 AND map_version < ?3",
                params![msg_id, n, MAP_VERSION],
            )?,
        };
        if changed == 0 {
            continue;
        }
        match verdict {
            Verdict::Show(message, info) => {
                messages.push(*message);
                infos.push(*info);
            }
            Verdict::Keep => done.kept += 1,
            Verdict::Skip => done.skipped += 1,
            Verdict::Unread => done.unread += 1,
        }
    }
    if let (Target::Conv(conv), false) = (target, messages.is_empty()) {
        // A take-up is no part of a push: it asks for nothing more and states no progress.
        let source = Source {
            msg_id: String::new(),
            sync_type: Some(progress::sync::ON_DEMAND),
            progress: None,
        };
        let (messages, infos, dropped) = checked_messages(conv, &source, messages, infos);
        done.dropped = dropped;
        if !messages.is_empty() {
            // The chat as the item was kept under is the closest to the chunk's own id that is left; upstream's own
            // seeding wrote the secret under the chunk's id when it could.
            let entries: Vec<_> = infos
                .iter()
                .flat_map(|info| {
                    secrets::entries(conv_text, info, own.pn.as_ref(), own.lid.as_ref())
                })
                .collect();
            secret::put_all(tx, &entries)?;
            done.delivered = u32::try_from(messages.len()).unwrap_or(u32::MAX);
            let mut events = crate::appstate::zone::after_history(tx, conv, &messages)?;
            events.push(batch(conv, &source, messages));
            let encoded = encode_outbox_row(&events)
                .map_err(|_| error::invalid("a history row could not be encoded"))?;
            done.seq = Some(
                outbox::append(tx, &encoded, now)
                    .map_err(|_| error::database("append a history row failed"))?,
            );
        }
    }
    exec(
        tx,
        "count a taken-up history row",
        "UPDATE history_counts SET delivered = delivered + ?1, dropped = dropped + ?2, bad = bad + ?3 \
         WHERE id = 1",
        params![done.delivered, done.dropped, done.unread],
    )?;
    fault::hit(fault::HISTORY_ROW)?;
    Ok(done)
}

/// `history_deferred.map_version` of a kept part that was TAKEN UP and stays (kind 1, the call-log records, D13): the
/// mark is "done", not a map version, so it never compares below a later build's [`MAP_VERSION`] and no later map takes
/// the part up again. The part itself is kept (a core `Rebuild` cannot fetch the history again).
pub(crate) const TAKEN_UP: i64 = i64::MAX;

/// A `Rebuild` of the core (fix F5, DUR-2): the rebuilt core has none of the call lines the records made, and the
/// records are kept for exactly this case, so every kept record that was taken up is due again at this build's map
/// version (the next quiet step places it again; the core's `join` and `call_lines` make a repeat one line). The
/// number of records made due. Runs inside the zone that states `AccountReady` again.
pub(crate) fn retake_calls(tx: &Transaction<'_>) -> Result<usize, StoreError> {
    exec(
        tx,
        "make the kept call records due again",
        "UPDATE history_deferred SET map_version = ?2 WHERE kind = 1 AND map_version = ?1",
        params![TAKEN_UP, MAP_VERSION],
    )
}

/// Most call-log records one pass of [`take_up_calls`] places (one `core_outbox` row holds their events).
const CALLS_PER_PASS: i64 = 20;

/// What one pass of [`take_up_calls`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct TakenCalls {
    /// The row's place in `core_outbox`, when a line was delivered (wake the forwarder).
    pub(crate) seq: Option<i64>,
    /// What the records came to (lines, silent, unplaced, refused): [`call_log::place`]'s verdicts.
    pub(crate) counts: call_log::Counts,
    /// Records that do not name their call, creator or start (or do not decode): marked taken up, no line.
    pub(crate) unread: u32,
}

/// A kept call-log record not yet taken up, as read.
struct DueCall {
    msg_id: String,
    n: i64,
    bytes: Vec<u8>,
}

/// Takes up ONE pass of the call-log records the history push kept (`history_deferred` kind 1, D13): the oldest kept
/// ones not yet marked [`TAKEN_UP`], at most [`CALLS_PER_PASS`]. Each becomes a quiet call line in the chat of its call
/// ([`call_log::place`]: the chat is added when the core has none), ALL in one `core_outbox` row, in the transaction
/// that marks them, so each is placed once whatever ends the process. A record the build cannot place is marked too (it
/// would fail again). The caller runs it only once the push is over. `None`: nothing is due.
pub(crate) async fn take_up_calls(
    store: &KourierStore,
    own: OwnIds,
    now_ms: i64,
) -> Result<Option<TakenCalls>, StoreError> {
    // A record's direction is its creator compared with BOTH of our ids: with one unknown it could file a call of ours
    // under our own number. The records wait (they are kept) until the device row has both.
    if own.lid.is_none() || own.pn.is_none() {
        return Ok(None);
    }
    let due = store.read(due_calls).await?;
    if due.is_empty() {
        return Ok(None);
    }
    // Mapping is pure and happens outside the zone.
    let records: Vec<(String, i64, Option<call_log::Seen>)> = due
        .into_iter()
        .map(|item| {
            let seen = wa::CallLogRecord::decode_from_slice(&item.bytes)
                .ok()
                .and_then(|record| call_log::Seen::from_record(&record, &own));
            (item.msg_id, item.n, seen)
        })
        .collect();
    store
        .sweep_at("take up the call log of the history", POINTS, move |tx| {
            take_up_calls_zone(tx, &own, records, now_ms)
        })
        .await
        .map(Some)
}

/// The oldest call-log records not yet taken up.
fn due_calls(conn: &Connection) -> Result<Vec<DueCall>, StoreError> {
    let mut stmt = conn
        .prepare_cached(
            "SELECT chunk_msg_id, n, item FROM history_deferred \
             WHERE kind = 1 AND map_version < ?1 ORDER BY chunk_msg_id, n LIMIT ?2",
        )
        .map_err(sql("look for kept call records"))?;
    let rows = stmt
        .query_map(params![TAKEN_UP, CALLS_PER_PASS], |r| {
            Ok(DueCall {
                msg_id: r.get(0)?,
                n: r.get(1)?,
                bytes: r.get(2)?,
            })
        })
        .map_err(sql("read kept call records"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(sql("read kept call records"))
}

/// The zone of [`take_up_calls`]: the records take the [`TAKEN_UP`] mark, their lines (and the statements of the chats
/// they add) go into ONE `core_outbox` row.
fn take_up_calls_zone(
    tx: &Transaction<'_>,
    own: &OwnIds,
    records: Vec<(String, i64, Option<call_log::Seen>)>,
    now: i64,
) -> Result<TakenCalls, StoreError> {
    let mut done = TakenCalls::default();
    let mut events: Vec<BackendEvent> = Vec::new();
    for (msg_id, n, seen) in records {
        // The record must still be due: one marked meanwhile is not ours to place.
        let changed = exec(
            tx,
            "mark a kept call record taken up",
            "UPDATE history_deferred SET map_version = ?3 \
             WHERE chunk_msg_id = ?1 AND n = ?2 AND kind = 1 AND map_version < ?3",
            params![msg_id, n, TAKEN_UP],
        )?;
        if changed == 0 {
            continue;
        }
        let Some(seen) = seen else {
            done.unread += 1;
            continue;
        };
        let placed = call_log::place(tx, &seen, own, now)?;
        done.counts.add(placed.verdict);
        events.extend(placed.events);
    }
    if !events.is_empty() {
        let row = encode_outbox_row(&events)
            .map_err(|_| error::invalid("a history row could not be encoded"))?;
        done.seq = Some(
            outbox::append(tx, &row, now)
                .map_err(|_| error::database("append a history row failed"))?,
        );
    }
    fault::hit(fault::HISTORY_ROW)?;
    Ok(done)
}

/// `history_delivery.kind` of a row (the number of the phase it belongs to; 3 is the end rows of a chunk).
pub(crate) const KIND_PEOPLE: i64 = 0;
pub(crate) const KIND_CHATS: i64 = 1;
pub(crate) const KIND_MESSAGES: i64 = 2;
pub(crate) const KIND_END: i64 = 3;

/// The zone body: the look, what the row does, and the cursor, so that it commits as one.
fn zone(
    tx: &Transaction<'_>,
    source: &Source,
    own: &Own,
    row: Row,
    now: i64,
) -> Result<Outcome, StoreError> {
    if !stands_at(tx, row.before)? {
        return Ok(Outcome::OutOfStep);
    }
    let Row {
        before,
        after,
        body,
    } = row;
    let done = write_row(tx, source, own, (before, after), body, now)?;
    exec(
        tx,
        "move the history cursor",
        "UPDATE history_cursor SET chunk = ?1, phase = ?2, conv = ?3, msg = ?4 WHERE id = 1",
        params![after.chunk, after.phase.number(), after.conv, after.msg],
    )?;
    // The seam of a failing zone: everything written, nothing committed.
    fault::hit(fault::HISTORY_ROW)?;
    Ok(Outcome::Appended(done))
}

/// Appends ONE unit of the taken-back row `entry` (`history_delivery.id`, `seq` NULL): `row` is what
/// [`Reader::next`](super::rows::Reader::next) read at the entry's place with the single bounds. The look is at
/// the entry (it must still wait, and begin exactly where `row` does), not at the cursor, which stays.
pub(crate) async fn redo(
    store: &KourierStore,
    source: Source,
    own: Own,
    entry: i64,
    row: Row,
    now_ms: i64,
) -> Result<Outcome, StoreError> {
    store
        .sweep_at("append a taken-back history row", POINTS, move |tx| {
            redo_zone(tx, &source, &own, entry, row, now_ms)
        })
        .await
}

fn redo_zone(
    tx: &Transaction<'_>,
    source: &Source,
    own: &Own,
    entry: i64,
    row: Row,
    now: i64,
) -> Result<Outcome, StoreError> {
    let Some(parent) = settle::pending(tx, entry)? else {
        return Ok(Outcome::OutOfStep);
    };
    if parent.begin() != row.before {
        return Ok(Outcome::OutOfStep);
    }
    let Row {
        before,
        after,
        body,
    } = row;
    let done = write_row(tx, source, own, (before, after), body, now)?;
    // The taken-back row is redone as far as `after`: finished once it reached its end, else it begins there.
    settle::advance(tx, &parent, after)?;
    fault::hit(fault::HISTORY_ROW)?;
    Ok(Outcome::Appended(done))
}

/// What a row does in its zone, but for the cursor: its kept items, secrets and chat states, the ONE
/// `core_outbox` row for its events with the entry that names its place, and the counts.
/// `(before, after)` are the positions the row was read between.
fn write_row(
    tx: &Transaction<'_>,
    source: &Source,
    own: &Own,
    (before, after): (Cursor, Cursor),
    body: Body,
    now: i64,
) -> Result<Appended, StoreError> {
    let mut done = Appended::default();
    // The newest pair recorded before this zone wrote any: the pairs above it are this row's (G2, `claim_since` below).
    let pairs_before = crate::store::merges::mark(tx)?;
    let (kind, conv, events, units) = match body {
        Body::People { events, parts } => {
            for kept in &parts {
                done.deferred += keep_part(tx, source, kept)?;
            }
            let units = events.len();
            // The chunk's people are stated for the people the core knows; the rest waits in the store for the day it
            // knows them (F11, `strangers`), and every pair stated is recorded for the merge sweep in this zone (F12).
            let (events, dropped) =
                checked_each(crate::strangers::scope(tx, leave_named_alone(tx, events)?)?);
            if dropped > 0 {
                tracing::error!(
                    dropped,
                    "history people events did not pass the contract check"
                );
            }
            crate::merges::record(tx, &events)?;
            (KIND_PEOPLE, String::new(), events, units)
        }
        Body::Chats(chats) => {
            let units = chats.len();
            let events = chats_events(tx, source, before.chunk, chats)?;
            (KIND_CHATS, String::new(), events, units)
        }
        Body::Messages {
            conv,
            place,
            chat_id,
            pieces,
        } => {
            let (events, text, units) = messages_events(
                tx,
                source,
                own,
                (conv, &place.jid, &place.target, &chat_id),
                pieces,
                &mut done,
            )?;
            (KIND_MESSAGES, text, events, units)
        }
        Body::End { finished, convs } => {
            if let Some(end) = finished {
                record_chunk(tx, before.chunk, end)?;
            }
            let events = end_events(tx, &convs)?;
            (KIND_END, String::new(), events, convs.len())
        }
    };
    if !events.is_empty() {
        let encoded = encode_outbox_row(&events)
            .map_err(|_| error::invalid("a history row could not be encoded"))?;
        let seq = outbox::append(tx, &encoded, now)
            .map_err(|_| error::database("append a history row failed"))?;
        // The place a take-back needs: where the row began and where it left.
        exec(
            tx,
            "record a history delivery row",
            "INSERT INTO history_delivery(seq, chunk, kind, place, conv, first, end_phase, \
                end_place, end_first, single, messages) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                seq,
                before.chunk,
                kind,
                if kind == KIND_MESSAGES {
                    i64::from(before.conv)
                } else {
                    0
                },
                conv,
                before.msg,
                after.phase.number(),
                after.conv,
                after.msg,
                // An end row is never cut further: a refused one is dropped (the viewmodel's idle rule ends
                // the banner it would have ended).
                i64::from(units == 1 || kind == KIND_END),
                done.delivered,
            ],
        )?;
        done.seq = Some(seq);
        // The pairs this row's zone recorded as told are this row's: its take-back forgets those and no others (G2).
        crate::store::merges::claim_since(tx, pairs_before, seq)?;
    }
    exec(
        tx,
        "count a history row",
        "UPDATE history_counts SET delivered = delivered + ?1, dropped = dropped + ?2, \
            deferred = deferred + ?3 WHERE id = 1",
        params![done.delivered, done.dropped, done.deferred],
    )?;
    Ok(done)
}

/// Whether the stored cursor stands where a row begins: exactly there, or (a new chunk) at the start of a
/// chunk other than the stored one once the stored one is finished, or while none was started.
fn stands_at(conn: &Connection, before: Cursor) -> Result<bool, StoreError> {
    let (chunk, phase, conv, msg): (i64, i64, i64, i64) = conn
        .query_row(
            "SELECT chunk, phase, conv, msg FROM history_cursor WHERE id = 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .map_err(sql("read the history cursor"))?;
    let asked = (
        before.chunk,
        before.phase.number(),
        i64::from(before.conv),
        i64::from(before.msg),
    );
    if (chunk, phase, conv, msg) == asked {
        return Ok(true);
    }
    let begins = before == Cursor::start(before.chunk) && before.chunk != chunk;
    Ok(begins && (chunk == 0 || phase == Phase::End.number()))
}

/// `(events, dropped)`: the events that pass the contract check, each judged alone.
fn checked_each(events: Vec<BackendEvent>) -> (Vec<BackendEvent>, u32) {
    let mut kept = Vec::with_capacity(events.len());
    let mut dropped = 0;
    for event in events {
        let (mut one, ok) = valid(vec![event]);
        if ok {
            kept.append(&mut one);
        } else {
            dropped += 1;
        }
    }
    (kept, dropped)
}

/// The events back, and whether they pass the contract's envelope check as one durable envelope.
fn valid(events: Vec<BackendEvent>) -> (Vec<BackendEvent>, bool) {
    let (envelope, _waiter) = Envelope::durable(events);
    let ok = envelope.validate().is_ok();
    (envelope.events, ok)
}

/// Keeps a part of the chunk that is no message (`history_deferred`); 1 when it is new.
fn keep_part(tx: &Transaction<'_>, source: &Source, kept: &Kept) -> Result<u32, StoreError> {
    keep(
        tx,
        source,
        kept.n,
        &kept.part.conv,
        &kept.part.item,
        kept.part.kind as u8,
    )
}

fn keep(
    tx: &Transaction<'_>,
    source: &Source,
    n: i64,
    conv: &str,
    item: &[u8],
    kind: u8,
) -> Result<u32, StoreError> {
    let changed = exec(
        tx,
        "keep a history item for a later build",
        "INSERT INTO history_deferred(chunk_msg_id, n, conv, item, map_version, kind) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6) ON CONFLICT (chunk_msg_id, n) DO NOTHING",
        params![source.msg_id, n, conv, item, MAP_VERSION, kind],
    )?;
    Ok(u32::from(changed > 0))
}

/// The events of a chats row, and the states of its chats written: the pair and the upsert of each chat,
/// with the hint decided now. A chat whose events fail the contract check is left out whole, and its state
/// is not written (a later chunk may bring it again).
fn chats_events(
    tx: &Transaction<'_>,
    source: &Source,
    chunk: i64,
    chats: Vec<Chat>,
) -> Result<Vec<BackendEvent>, StoreError> {
    let mut out = Vec::new();
    let mut dropped = 0u32;
    for chat in &chats {
        let key = chat.conv.encode();
        let state = chat_state(tx, &key)?;
        let live = live::count_up_to(tx, &key, chat.live_cut_ms)?;
        let hint = chat::hint(chat.unread, live, state);
        let mut events = chat.events(hint);
        // A title the live path stated since the link is newer than the chunk's (F3).
        if named::titled(tx, &key)? {
            for event in &mut events {
                if let BackendEvent::ConversationUpsert(patch) = event {
                    patch.title = Patch::Unchanged;
                }
            }
        }
        let (events, ok) = valid(events);
        if !ok {
            dropped += 1;
            continue;
        }
        // Only a sync type that pushes chats owes the chat an end; a chat an earlier push left open stays open.
        let more = i64::from(progress::pushes_chats(source.sync_type));
        exec(
            tx,
            "record a chat the history touched",
            "INSERT INTO history_chats(conv, more, hint_stated, last_chunk, sync_type, ended) \
             VALUES (?1, ?5, ?2, ?3, ?4, ?6) \
             ON CONFLICT (conv) DO UPDATE SET more = max(more, excluded.more), \
               hint_stated = max(hint_stated, excluded.hint_stated), \
               last_chunk = excluded.last_chunk, sync_type = excluded.sync_type, \
               ended = excluded.ended",
            params![
                key,
                i64::from(hint.is_some()),
                chunk,
                source.sync_type,
                more,
                i64::from(chat.ended),
            ],
        )?;
        // The chunk carries a real message of the chat, from either side: it was started on the phone (W5a, D7 as
        // amended by fix F2). A chats row alone is no start: a stranger's call can make one.
        if chat.real {
            crate::send::started::note(tx, &chat.conv, crate::send::started::How::History)?;
        }
        // Its trusted-contact token, kept in THIS zone (D12): a kill after the chunk's receipt cannot leave the chat
        // without it (upstream stores it only after the receipt).
        if let Some(token) = &chat.token {
            keep_token(tx, token)?;
        }
        // And its features are stated with its row, with the fact just noted (D9).
        match crate::features::with_statements(tx, &events)? {
            Some(stated) => out.extend(stated),
            None => out.extend(events),
        }
    }
    if dropped > 0 {
        tracing::error!(dropped, "history chats did not pass the contract check");
    }
    Ok(out)
}

/// A chat's token under upstream's key rule (`store_tc_token_candidate`, which learns the chunk's pairs first): the
/// LID user the chunk states, else the current LID of the number by the store's `lid_pn_mapping` (the newest row,
/// as upstream's `get_current_lid`), else the number's user.
fn keep_token(tx: &Transaction<'_>, token: &chat::Token) -> Result<(), StoreError> {
    let key = match &token.lid {
        Some(lid) => lid.clone(),
        None => tx
            .prepare_cached(
                "SELECT lid FROM lid_pn_mapping WHERE phone = ?1 \
                 ORDER BY updated_at DESC, rowid DESC LIMIT 1",
            )
            .and_then(|mut s| {
                s.query_row([&token.user], |r| r.get::<_, String>(0))
                    .optional()
            })
            .map_err(sql("read the current lid of a history chat"))?
            .unwrap_or_else(|| token.user.clone()),
    };
    crate::store::keep_history_token(tx, &key, &token.bytes, token.at, token.sender_at)
}

/// A chunk's push names, but not for the people the live path stated a push name of since the link (F3): their
/// name is newer than the snapshot, so the upsert says `Unchanged` for it, and an upsert that then says nothing at
/// all is left out.
fn leave_named_alone(
    tx: &Transaction<'_>,
    events: Vec<BackendEvent>,
) -> Result<Vec<BackendEvent>, StoreError> {
    let users: Vec<&str> = events
        .iter()
        .filter_map(|event| match event {
            BackendEvent::ContactUpsert(patch) if matches!(patch.profile_name, Patch::Set(_)) => {
                Some(patch.user.as_str())
            }
            _ => None,
        })
        .collect();
    if users.is_empty() {
        return Ok(events);
    }
    let live_named = named::named(tx, &users)?;
    if live_named.is_empty() {
        return Ok(events);
    }
    Ok(events
        .into_iter()
        .filter_map(|event| match event {
            BackendEvent::ContactUpsert(mut patch) if live_named.contains(patch.user.as_str()) => {
                patch.profile_name = Patch::Unchanged;
                // A contact upsert with no field to change says nothing.
                (patch != kourier_model::ContactPatch::new(patch.user.clone()))
                    .then_some(BackendEvent::ContactUpsert(patch))
            }
            other => Some(other),
        })
        .collect())
}

/// What the store knows of a chat's hint.
fn chat_state(conn: &Connection, key: &str) -> Result<HintState, StoreError> {
    let row: Option<(i64, i64)> = conn
        .prepare_cached("SELECT hint_stated, read_here FROM history_chats WHERE conv = ?1")
        .and_then(|mut s| {
            s.query_row([key], |r| Ok((r.get(0)?, r.get(1)?)))
                .optional()
        })
        .map_err(sql("read the state of a history chat"))?;
    Ok(
        row.map_or_else(HintState::default, |(stated, read_here)| HintState {
            stated: stated != 0,
            read_here: read_here != 0,
        }),
    )
}

/// The events of a messages row (one `HistoryBatch`, or none), with the row's secrets and kept items written.
/// `(plan place, id the chat was placed by, where it leads, the chunk's own id of it)` names the chat.
/// Returns `(events, the conversation text, units)`.
fn messages_events(
    tx: &Transaction<'_>,
    source: &Source,
    own: &Own,
    chat: (u32, &str, &Target, &str),
    pieces: Vec<Piece>,
    done: &mut Appended,
) -> Result<(Vec<BackendEvent>, String, usize), StoreError> {
    let (place_n, placed_by, target, chat_id) = chat;
    let units = pieces.len();
    let mut messages: Vec<IncomingMessage> = Vec::new();
    let mut infos: Vec<wa::WebMessageInfo> = Vec::new();
    let mut kept_for: BTreeMap<&'static str, u32> = BTreeMap::new();
    for Piece { index, info, item } in pieces {
        match item {
            Item::Message(message) => {
                messages.push(*message);
                infos.push(info);
            }
            Item::Deferred(reason) => {
                done.deferred += keep(
                    tx,
                    source,
                    deferred_n(place_n, index),
                    placed_by,
                    &info.encode_to_vec(),
                    Kind::Message as u8,
                )?;
                *kept_for.entry(reason).or_insert(0u32) += 1;
            }
            Item::Skip => {}
        }
    }
    // What this build could not show, by word (a static one, log-safe), never by item.
    for (reason, count) in kept_for {
        tracing::debug!(reason, count, "history items kept for a later build");
    }
    // A chat this build has no conversation for carries only kept items.
    let Target::Conv(conv) = target else {
        return Ok((Vec::new(), String::new(), units));
    };
    if messages.is_empty() {
        return Ok((Vec::new(), conv.encode(), units));
    }
    let (messages, infos, dropped) = checked_messages(conv, source, messages, infos);
    done.dropped += dropped;
    if messages.is_empty() {
        return Ok((Vec::new(), conv.encode(), units));
    }
    // The secrets go in THIS zone: they are there exactly when the messages are in the core.
    let entries: Vec<_> = infos
        .iter()
        .flat_map(|info| secrets::entries(chat_id, info, own.pn.as_ref(), own.lid.as_ref()))
        .collect();
    secret::put_all(tx, &entries)?;
    done.delivered += u32::try_from(messages.len()).unwrap_or(u32::MAX);
    // The newest message that counts is noted in this zone, and an archive it is newer than goes (F1).
    let mut events = crate::appstate::zone::after_history(tx, conv, &messages)?;
    events.push(batch(conv, source, messages));
    Ok((events, conv.encode(), units))
}

fn batch(conv: &ConvKey, source: &Source, messages: Vec<IncomingMessage>) -> BackendEvent {
    // The chunk's own progress rides on a push's batches; an answer to a request (ON_DEMAND) is no part of it.
    let pushes = progress::pushes_chats(source.sync_type);
    BackendEvent::HistoryBatch {
        conv: conv.clone(),
        messages,
        has_more: pushes,
        progress: if pushes { source.progress } else { None },
    }
}

/// The chunk is delivered (its content is all appended): what the end of a push needs of it, and the chats of
/// it that did not decode counted.
fn record_chunk(tx: &Transaction<'_>, chunk: i64, end: ChunkEnd) -> Result<(), StoreError> {
    exec(
        tx,
        "record a delivered history chunk",
        "INSERT INTO history_chunks(chunk, sync_type, chunk_order, progress) \
         VALUES (?1, ?2, ?3, ?4) ON CONFLICT (chunk) DO NOTHING",
        params![chunk, end.sync_type, end.order, end.progress],
    )?;
    if end.skipped > 0 {
        exec(
            tx,
            "count the chats of a chunk that did not read",
            "UPDATE history_counts SET bad = bad + ?1 WHERE id = 1",
            [end.skipped],
        )?;
    }
    Ok(())
}

/// The end of the push for `convs`: one empty batch each, `has_more: false` (the chat's `more` cleared in
/// the same transaction, so the end is stated exactly once).
fn end_events(tx: &Transaction<'_>, convs: &[ConvKey]) -> Result<Vec<BackendEvent>, StoreError> {
    let mut events = Vec::with_capacity(convs.len());
    for conv in convs {
        let changed = exec(
            tx,
            "end a chat of the history push",
            "UPDATE history_chats SET more = 0 WHERE conv = ?1 AND more = 1",
            [conv.encode()],
        )?;
        if changed > 0 {
            events.push(BackendEvent::HistoryBatch {
                conv: conv.clone(),
                messages: Vec::new(),
                has_more: false,
                progress: None,
            });
        }
    }
    Ok(events)
}

/// The messages (with their entries) that pass the contract check: the whole batch at once, else each alone.
/// The third is how many did not.
fn checked_messages(
    conv: &ConvKey,
    source: &Source,
    messages: Vec<IncomingMessage>,
    infos: Vec<wa::WebMessageInfo>,
) -> (Vec<IncomingMessage>, Vec<wa::WebMessageInfo>, u32) {
    let (mut whole, ok) = valid(vec![batch(conv, source, messages)]);
    let Some(BackendEvent::HistoryBatch { messages, .. }) = whole.pop() else {
        return (Vec::new(), Vec::new(), 0);
    };
    if ok {
        return (messages, infos, 0);
    }
    let (mut kept, mut kept_infos, mut dropped) = (Vec::new(), Vec::new(), 0u32);
    for (message, info) in messages.into_iter().zip(infos) {
        let (mut alone, ok) = valid(vec![batch(conv, source, vec![message])]);
        match alone.pop() {
            Some(BackendEvent::HistoryBatch { mut messages, .. }) if ok => {
                kept.append(&mut messages);
                kept_infos.push(info);
            }
            _ => dropped += 1,
        }
    }
    (kept, kept_infos, dropped)
}

#[cfg(test)]
mod tests;
