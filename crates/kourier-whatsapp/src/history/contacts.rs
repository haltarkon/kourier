//! The address-book names the history push kept (W6a2, B8b; decision D8 of `target/wa/w6-decisions.md`): `history_deferred`
//! kind 3, the phone's `InlineContact`s.
//!
//! Taken up ONCE, after the push is over (the driver's quiet step, like the call log): each entry goes into the address-book
//! mirror ([`crate::book::apply_older`]; a contact the app state already named is left as it is, the live word is fresher)
//! and the names of the people the core knows are stated, all of one pass in ONE `core_outbox` row, in the transaction that
//! deletes the kept entries. Deleted, not marked: the mirror IS the keeping (a `Rebuild` restates names from it, and the
//! phone's address book is not a thing the history can be asked for again), so no second copy of the names is held.
//!
//! Nothing here logs a name, an id or a number; the driver counts.

use kourier_backend_api::{BackendEvent, encode_outbox_row};
use kourier_db::outbox;
use kourier_db::rusqlite::Transaction;
use kourier_db::rusqlite::{Connection, params};
use wacore::store::error::StoreError;
use waproto::buffa::Message as _;
use waproto::whatsapp as wa;

use super::rest::Kind;
use crate::book;
use crate::db::Points;
use crate::error::{self, sql};
use crate::failpoints::{AFTER_HISTORY_ROW, INSIDE_HISTORY_ROW};
use crate::fault;
use crate::store::{KourierStore, exec};

const POINTS: Points = Points {
    inside: INSIDE_HISTORY_ROW,
    after: AFTER_HISTORY_ROW,
};

/// Most kept entries one pass takes (one `core_outbox` row holds their events).
const PER_PASS: i64 = 200;

/// What one pass did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Taken {
    /// The row's place in `core_outbox`, when a name was stated (wake the forwarder).
    pub(crate) seq: Option<i64>,
    /// Entries that went into the mirror (or were left to the app state's fresher word).
    pub(crate) taken: u32,
    /// Entries that name nobody or do not decode: dropped.
    pub(crate) unread: u32,
}

/// A kept entry not yet taken up, as read.
struct Due {
    msg_id: String,
    n: i64,
    bytes: Vec<u8>,
}

/// Takes up ONE pass of the kept address-book entries. `None`: nothing is due.
pub(crate) async fn take_up(
    store: &KourierStore,
    now_ms: i64,
) -> Result<Option<Taken>, StoreError> {
    let due = store.read(due).await?;
    if due.is_empty() {
        return Ok(None);
    }
    // Mapping is pure and happens outside the zone.
    let entries: Vec<(String, i64, Option<book::Change>)> = due
        .into_iter()
        .map(|item| {
            let change = wa::InlineContact::decode_from_slice(&item.bytes)
                .ok()
                .and_then(|contact| book::change_of_inline(&contact));
            (item.msg_id, item.n, change)
        })
        .collect();
    store
        .sweep_at(
            "take up the address book of the history",
            POINTS,
            move |tx| zone(tx, entries, now_ms),
        )
        .await
        .map(Some)
}

/// The oldest kept entries.
fn due(conn: &Connection) -> Result<Vec<Due>, StoreError> {
    let mut stmt = conn
        .prepare_cached(
            "SELECT chunk_msg_id, n, item FROM history_deferred \
             WHERE kind = ?1 ORDER BY chunk_msg_id, n LIMIT ?2",
        )
        .map_err(sql("look for kept address-book entries"))?;
    let rows = stmt
        .query_map(params![Kind::Contact as u8, PER_PASS], |r| {
            Ok(Due {
                msg_id: r.get(0)?,
                n: r.get(1)?,
                bytes: r.get(2)?,
            })
        })
        .map_err(sql("read kept address-book entries"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(sql("read kept address-book entries"))
}

/// The zone of [`take_up`]: the entries leave `history_deferred`, the mirror takes them, their statements go into ONE
/// `core_outbox` row.
fn zone(
    tx: &Transaction<'_>,
    entries: Vec<(String, i64, Option<book::Change>)>,
    now: i64,
) -> Result<Taken, StoreError> {
    let mut done = Taken::default();
    let mut events: Vec<BackendEvent> = Vec::new();
    for (msg_id, n, change) in entries {
        // The entry must still be there: one taken meanwhile is not ours.
        let removed = exec(
            tx,
            "take a kept address-book entry",
            "DELETE FROM history_deferred WHERE chunk_msg_id = ?1 AND n = ?2 AND kind = ?3",
            params![msg_id, n, Kind::Contact as u8],
        )?;
        if removed == 0 {
            continue;
        }
        let Some(change) = change else {
            done.unread += 1;
            continue;
        };
        events.extend(book::apply_older(tx, &change)?);
        done.taken += 1;
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

#[cfg(test)]
mod tests;
