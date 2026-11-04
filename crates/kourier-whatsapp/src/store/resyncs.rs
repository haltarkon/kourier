//! The books of the start-up resync and the app-state map version (W6a1, chunk A2; decisions D2, D4, B1 and M7 of
//! `target/wa/w6-decisions.md`): `appstate_resyncs` and `appstate_map` of migration 7.
//!
//! PER COLLECTION, `appstate_resyncs` keeps when Kourier last asked the server for a snapshot (`asked_at`: the spacing
//! of the start-up resync counts from it; also stamped for every collection at the link, because the link session has
//! no snapshot of Kourier's), the GENERATION that snapshot opened (`gen`: [`asked`] hands out the next number; the
//! mirror's rows stamp it, see [`super::chat_state`]) and when the server answered it fatally (`fatal_at`: asking again
//! gets the same answer, so a collection with a fatal mark is asked again only after a new map version
//! ([`clear_fatal`]) or a new link ([`stamp_link`]), M7).
//!
//! `appstate_map` keeps the map version the store was last read with: a build whose map covers more verbs marks the
//! collections that grew dirty once (D4). No row reads as 0.
//!
//! `account_link` keeps the time of the pairing (F13): the books' `asked_at` is stamped with it at the link but moves with
//! every later snapshot of ours, so it cannot say when the store was linked.
//!
//! The functions run inside the caller's zone (`&Transaction`) or read (`&Connection`); none logs.

use kourier_db::rusqlite::{Connection, Transaction, params};
use wacore::store::error::StoreError;

use super::{exec, first, rows};

/// What the books say about one collection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Book {
    /// When Kourier last asked for a snapshot, or the link stamped the collection (ms); 0 for a row made by [`fatal`]
    /// alone.
    pub(crate) asked_at: i64,
    /// The generation the last snapshot opened (0: none yet).
    pub(crate) generation: i64,
    /// When the server answered a snapshot fatally; `None` once a new map version or a new link cleared it.
    pub(crate) fatal_at: Option<i64>,
}

/// The books of `collection`; `None` when Kourier never asked and the link never stamped it.
pub(crate) fn book(conn: &Connection, collection: &str) -> Result<Option<Book>, StoreError> {
    first(
        conn,
        "look up the resync books",
        "SELECT asked_at, gen, fatal_at FROM appstate_resyncs WHERE collection = ?1",
        params![collection],
        |r| {
            Ok(Book {
                asked_at: r.get(0)?,
                generation: r.get(1)?,
                fatal_at: r.get(2)?,
            })
        },
    )
}

/// Kourier asks `collection` for a snapshot at `now`: stamps the time and opens the next generation, which is
/// returned (1 for the first).
pub(crate) fn asked(tx: &Transaction<'_>, collection: &str, now: i64) -> Result<i64, StoreError> {
    const OP: &str = "record a resync";
    exec(
        tx,
        OP,
        "INSERT INTO appstate_resyncs(collection, asked_at, gen) VALUES (?1, ?2, 1) \
         ON CONFLICT (collection) DO UPDATE SET asked_at = excluded.asked_at, gen = appstate_resyncs.gen + 1",
        params![collection, now],
    )?;
    first(
        tx,
        OP,
        "SELECT gen FROM appstate_resyncs WHERE collection = ?1",
        params![collection],
        |r| r.get(0),
    )?
    .ok_or_else(|| crate::error::unreadable("the resync books"))
}

/// The server answered the snapshot of `collection` fatally at `now`; the collection is not asked again until
/// [`clear_fatal`] or [`stamp_link`].
pub(crate) fn fatal(tx: &Transaction<'_>, collection: &str, now: i64) -> Result<(), StoreError> {
    exec(
        tx,
        "record a fatal resync",
        "INSERT INTO appstate_resyncs(collection, fatal_at) VALUES (?1, ?2) \
         ON CONFLICT (collection) DO UPDATE SET fatal_at = excluded.fatal_at",
        params![collection, now],
    )
    .map(drop)
}

/// A new map version: every fatal mark goes, the collections may be asked again. The number of marks cleared.
pub(crate) fn clear_fatal(tx: &Transaction<'_>) -> Result<usize, StoreError> {
    exec(
        tx,
        "clear the fatal resyncs",
        "UPDATE appstate_resyncs SET fatal_at = NULL WHERE fatal_at IS NOT NULL",
        [],
    )
}

/// The link: the link session needs no snapshot of ours (upstream's initial full sync fills everything), so every
/// collection of `collections` counts as asked at `now` and its fatal mark goes. The generations stay.
pub(crate) fn stamp_link(
    tx: &Transaction<'_>,
    collections: &[&str],
    now: i64,
) -> Result<(), StoreError> {
    for collection in collections {
        exec(
            tx,
            "stamp the resync books at the link",
            "INSERT INTO appstate_resyncs(collection, asked_at) VALUES (?1, ?2) \
             ON CONFLICT (collection) DO UPDATE SET asked_at = excluded.asked_at, fatal_at = NULL",
            params![collection, now],
        )?;
    }
    Ok(())
}

/// The collections whose last snapshot the server answered fatally and that no new map version or link has cleared.
pub(crate) fn fatal_collections(conn: &Connection) -> Result<Vec<String>, StoreError> {
    rows(
        conn,
        "look up the fatal resyncs",
        "SELECT collection FROM appstate_resyncs WHERE fatal_at IS NOT NULL ORDER BY collection",
        [],
        |r| r.get(0),
    )
}

/// The app-state map version the store was last read with; 0 when never written.
pub(crate) fn map_version(conn: &Connection) -> Result<i64, StoreError> {
    Ok(first(
        conn,
        "look up the app-state map version",
        "SELECT version FROM appstate_map WHERE id = 1",
        [],
        |r| r.get(0),
    )?
    .unwrap_or(0))
}

/// Records the map version this build read the store with.
pub(crate) fn set_map_version(tx: &Transaction<'_>, version: i64) -> Result<(), StoreError> {
    exec(
        tx,
        "record the app-state map version",
        "INSERT INTO appstate_map(id, version) VALUES (1, ?1) \
         ON CONFLICT (id) DO UPDATE SET version = excluded.version",
        params![version],
    )
    .map(drop)
}

/// The link: when this store was paired (ms), the time of the scan. A new link of the same file rewrites it.
pub(crate) fn set_linked(tx: &Transaction<'_>, now: i64) -> Result<(), StoreError> {
    exec(
        tx,
        "record the link's time",
        "INSERT INTO account_link(id, linked_ms) VALUES (1, ?1) \
         ON CONFLICT (id) DO UPDATE SET linked_ms = excluded.linked_ms",
        params![now],
    )
    .map(drop)
}

/// When this store was paired (ms); `None` when it saw no link (F13: nothing counts as older than the link then).
pub(crate) fn linked_at(conn: &Connection) -> Result<Option<i64>, StoreError> {
    first(
        conn,
        "look up the link's time",
        "SELECT linked_ms FROM account_link WHERE id = 1",
        [],
        |r| r.get(0),
    )
}

#[cfg(test)]
mod tests;
