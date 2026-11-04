//! Which conversation a call's line was filed under (W6a1, chunk A6; decisions D12 and H2 of
//! `target/wa/w6-decisions.md`, critic L6): `call_lines` of migration 7.
//!
//! WHY. The call interceptor files an incoming 1:1 call under the creator as the stanza names it, and the phone's call
//! log may name the same person in the OTHER namespace (a number where the stanza said LID, or the reverse): a log that
//! chose its own conversation would make a second line for one call. So every call line is noted here in the zone that
//! writes it (the interceptor's by the event writer, the log's by `calls::log`), and the log's outcome lands in the
//! conversation and under the id the first line used. The first writer wins ([`note`]): a later signal that names the
//! same call id in another chat moves nothing.
//!
//! The rows are bookkeeping, not history: [`sweep`] forgets them 30 days after they were written (critic L6). A call
//! older than that whose log record comes again is placed by the log's own rules.
//!
//! The functions run inside the caller's zone (`&Transaction`) or read (`&Connection`); none logs.

use kourier_db::rusqlite::{Connection, Transaction, params};
use wacore::store::error::StoreError;

use super::{exec, first};

/// How long a row is kept (ms): 30 days after it was written.
pub(crate) const KEEP_MS: i64 = 30 * 24 * 3_600_000;

/// Notes that call `call_id` was filed under the conversation `conv` (its encoded key) at `at` (ms); `now` is when the
/// row is written. The first note of a call id stands. `true` when the row is new.
pub(crate) fn note(
    tx: &Transaction<'_>,
    call_id: &str,
    conv: &str,
    at: i64,
    now: i64,
) -> Result<bool, StoreError> {
    exec(
        tx,
        "note a call line",
        "INSERT INTO call_lines(call_id, conv, at, created_at) VALUES (?1, ?2, ?3, ?4) \
         ON CONFLICT (call_id) DO NOTHING",
        params![call_id, conv, at, now],
    )
    .map(|changed| changed > 0)
}

/// The conversation (its encoded key) call `call_id` was filed under, if a line of it was written.
pub(crate) fn conv_of(conn: &Connection, call_id: &str) -> Result<Option<String>, StoreError> {
    first(
        conn,
        "look up a call line",
        "SELECT conv FROM call_lines WHERE call_id = ?1",
        params![call_id],
        |r| r.get(0),
    )
}

/// Forgets the rows written more than [`KEEP_MS`] before `now`. How many went.
pub(crate) fn sweep(tx: &Transaction<'_>, now: i64) -> Result<usize, StoreError> {
    exec(
        tx,
        "forget old call lines",
        "DELETE FROM call_lines WHERE created_at < ?1",
        params![now.saturating_sub(KEEP_MS)],
    )
}

#[cfg(test)]
mod tests;
