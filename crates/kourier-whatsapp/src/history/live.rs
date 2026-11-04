//! The live messages the phone's unread count may include (W4 C10b, O2).
//!
//! WhatsApp states a chat's unread COUNT, never which messages are unread. A message of someone else
//! that reached us live after the link and is not newer than the chat's last message at the phone is
//! one the phone counts too, and the core already counts it as a row of its own: the hint the history
//! states for the chat is the phone's count minus those ([`super::chat::hint`]), or the badge would
//! show it twice.
//!
//! Stage 2 notes each such message in the zone that finishes its inbox row ([`record`]): by chat and by
//! server time, nothing of its content. The delivery reads the count in the zone that states the hint
//! ([`count_up_to`]).
//!
//! NOTES COST A ROW EACH, so they are written only while a push can still state a hint ([`wanted`]: a chat owes
//! an end, the stage holds a chunk not delivered yet, or an announced chunk is unresolved; W4 fix F4) and forgotten
//! in stage 2's own zone ([`tidy`]) as well as at the driver's start: a note is kept for [`KEEP_MS`] BEFORE the
//! oldest chunk that waits for its delivery (`history_cursor.live_floor`, the driver keeps it; W4 fix F5), because
//! the hint is decided at the delivery and a delivery that resumes days after the capture still needs the messages
//! that arrived before the chunk was made. `history_named` ([`super::named`]) is kept and forgotten the same way.
//!
//! The chat is the LIVE path's own conversation (the form the live map named it by); a history chat
//! placed by another form of the same person meets them only after the merge, so a person the phone
//! knows under two forms can be under-subtracted. That errs toward a hint that is one too high for a
//! chat the person reads anyway, never toward a hidden message.

use kourier_backend_api::BackendEvent;
use kourier_db::rusqlite::{Connection, Transaction};
use kourier_model::{Body, Origin};
use wacore::store::error::StoreError;

use crate::error::sql;

/// How long a note is kept: a push that starts later than this after a message has nothing to
/// subtract it from (the phone's last message is at most as old as the push is long, and a push is
/// minutes to hours).
pub(crate) const KEEP_MS: i64 = 24 * 60 * 60 * 1000;

/// Notes the messages of others among `events` (the events of one finished inbox row). Returns how
/// many were noted. Our own messages, notices and everything that is no message note nothing.
pub(crate) fn record(tx: &Transaction<'_>, events: &[BackendEvent]) -> Result<usize, StoreError> {
    let mut noted = 0;
    for event in events {
        let BackendEvent::Message(message) = event else {
            continue;
        };
        if !matches!(message.origin, Origin::Remote { .. })
            || matches!(message.body, Body::System(_))
        {
            continue;
        }
        tx.prepare_cached("INSERT INTO history_live(conv, sent_at) VALUES (?1, ?2)")
            .and_then(|mut s| s.execute((message.conv.encode(), message.sent_at)))
            .map_err(sql("note a live message for the unread hint"))?;
        noted += 1;
    }
    Ok(noted)
}

/// How many noted messages of `conv` were sent at or before `cut_ms`: the live messages the phone's
/// count of that chat includes.
pub(crate) fn count_up_to(conn: &Connection, conv: &str, cut_ms: i64) -> Result<u32, StoreError> {
    let n: i64 = conn
        .prepare_cached("SELECT count(*) FROM history_live WHERE conv = ?1 AND sent_at <= ?2")
        .and_then(|mut s| s.query_row((conv, cut_ms), |r| r.get(0)))
        .map_err(sql("count the live messages of a chat"))?;
    Ok(u32::try_from(n).unwrap_or(u32::MAX))
}

/// Forgets the notes sent before `before_ms`. Returns how many went.
pub(crate) fn prune(tx: &Transaction<'_>, before_ms: i64) -> Result<usize, StoreError> {
    tx.prepare_cached("DELETE FROM history_live WHERE sent_at < ?1")
        .and_then(|mut s| s.execute([before_ms]))
        .map_err(sql("forget old live messages"))
}

/// Whether a push can still state a hint or a name (module docs): then the live path notes what a chunk's
/// snapshot may overwrite or count again. One look, stage 2 reads it once per step.
pub(crate) fn wanted(conn: &Connection) -> Result<bool, StoreError> {
    conn.query_row(
        "SELECT (SELECT live_floor IS NOT NULL FROM history_cursor WHERE id = 1) \
             OR EXISTS (SELECT 1 FROM history_chats WHERE more = 1) \
             OR (SELECT count(*) FROM history_announced WHERE lost = 0) \
                > (SELECT count(*) FROM history_chunks)",
        [],
        |r| r.get::<_, bool>(0),
    )
    .map_err(sql("look whether a history push can still state a hint"))
}

/// The time before which the notes are no use any more: [`KEEP_MS`] before the oldest chunk that waits for its
/// delivery (`history_cursor.live_floor`), or before `now_ms` when none waits.
pub(crate) fn horizon(conn: &Connection, now_ms: i64) -> Result<i64, StoreError> {
    let floor: Option<i64> = conn
        .query_row(
            "SELECT live_floor FROM history_cursor WHERE id = 1",
            [],
            |r| r.get(0),
        )
        .map_err(sql("read the floor of the live notes"))?;
    Ok(floor.map_or(now_ms, |floor| floor.min(now_ms)) - KEEP_MS)
}

/// Forgets the notes that are no use any more ([`horizon`]) of both tables, in the caller's zone. Returns how many
/// notes of live messages went.
pub(crate) fn tidy(tx: &Transaction<'_>, now_ms: i64) -> Result<usize, StoreError> {
    let before = horizon(tx, now_ms)?;
    super::named::prune(tx, before)?;
    prune(tx, before)
}

/// Sets when the oldest chunk that waits for its delivery was captured (`None`: no chunk waits).
pub(crate) fn set_floor(tx: &Transaction<'_>, floor_ms: Option<i64>) -> Result<(), StoreError> {
    tx.execute(
        "UPDATE history_cursor SET live_floor = ?1 WHERE id = 1",
        [floor_ms],
    )
    .map(drop)
    .map_err(sql("set the floor of the live notes"))
}

/// Forgets every note: the push is over (its last chat got its end), so no count needs them. Only the tests
/// call it: a later push (another sync type) still needs the notes for the chats it brings first, so the
/// driver forgets by age ([`prune`]) and never all at once.
#[cfg(test)]
pub(crate) fn clear(tx: &Transaction<'_>) -> Result<usize, StoreError> {
    tx.execute("DELETE FROM history_live", [])
        .map_err(sql("forget the live messages"))
}

#[cfg(test)]
mod tests;
