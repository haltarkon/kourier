//! The phone's address book as a mirror, and the people the core knows (W6a2, chunk B8b; decision D8 of
//! `target/wa/w6-decisions.md`): the tables `address_book` and `persons_known` of migration 8.
//!
//! `address_book` is what the PHONE said, not what the core was told: a `Rebuild` does not forget it (the phone does not
//! say it again), and it goes with the account's file. A row the app state said carries the generation of its collection
//! like a chat setting ([`put_from`] stamps it, [`unseen`] finds the rows a snapshot left behind). `persons_known` IS a statement ("the core was told of this person as a
//! chat's peer, a group's member or a message's author") and is forgotten with the others
//! ([`super::people::forget_statements`]). Names are text a person typed: nothing here logs, and a row is only read and
//! written by encoded ids and the name.
//!
//! The functions run inside the caller's zone (`&Transaction`) or read (`&Connection`).

use kourier_db::rusqlite::{Connection, Transaction, params};
use wacore::store::error::StoreError;

use super::{exec, first, rows};

/// One contact of the phone's address book, in encoded ids. `Debug` prints nothing of it.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Row {
    /// The mutation's own index id.
    pub(crate) contact: String,
    /// The contact's name (full, else first).
    pub(crate) name: String,
    /// The `pn:` form of the person, when the row names one.
    pub(crate) pn: Option<String>,
    /// The `lid:` form of the person, when the row names one.
    pub(crate) lid: Option<String>,
}

impl std::fmt::Debug for Row {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Row(..)")
    }
}

/// Who said an address-book row (`address_book.origin`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Source {
    /// The app state's live `contact` mutation: the phone's current word.
    AppState = 0,
    /// The history's `InlineContact` (kind 3): older, taken up long after it was written.
    History = 1,
}

/// The phone said `row` through the app state: [`put_from`] with [`Source::AppState`] at generation 0 (the tests' shorthand).
#[cfg(test)]
pub(crate) fn put(tx: &Transaction<'_>, row: &Row) -> Result<bool, StoreError> {
    put_from(tx, row, Source::AppState, 0)
}

/// `source` said `row` at the collection's `generation`: the mirror's row is made or replaced and stamped with the
/// generation, also when nothing else changed (a replay that says the same thing again is a touch: [`unseen`] finds what
/// was not). `true` when that is a change (a new row or another value), not when only the stamp moved.
pub(crate) fn put_from(
    tx: &Transaction<'_>,
    row: &Row,
    source: Source,
    generation: i64,
) -> Result<bool, StoreError> {
    let changed = exec(
        tx,
        "record an address-book contact",
        "INSERT INTO address_book(contact, name, pn, lid, origin, gen) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT (contact) DO UPDATE SET name = excluded.name, pn = excluded.pn, lid = excluded.lid,
           origin = excluded.origin, gen = excluded.gen
           WHERE address_book.name IS NOT excluded.name OR address_book.pn IS NOT excluded.pn
              OR address_book.lid IS NOT excluded.lid OR address_book.origin IS NOT excluded.origin",
        params![row.contact, row.name, row.pn, row.lid, source as i64, generation],
    )? > 0;
    // The same word again: only the stamp moves.
    exec(
        tx,
        "stamp an address-book contact",
        "UPDATE address_book SET gen = ?2 WHERE contact = ?1 AND gen IS NOT ?2",
        params![row.contact, generation],
    )?;
    Ok(changed)
}

/// The contacts a snapshot of `generation` did not mention: the app state's rows (origin 0) still on an older generation,
/// by index id. They are what the phone no longer has (W6a2 F19, DUR-6).
pub(crate) fn unseen(conn: &Connection, generation: i64) -> Result<Vec<String>, StoreError> {
    rows(
        conn,
        "look up the contacts a snapshot did not mention",
        "SELECT contact FROM address_book WHERE origin = 0 AND gen < ?1 ORDER BY contact",
        params![generation],
        |r| r.get::<_, String>(0),
    )
}

/// The live word named `forms` (the ids of one person): the history's rows that name any of them go. They are older than
/// the word and describe the same person, so they must neither win over it nor come back as his name when it is taken away
/// (W6a2 PRIV-4). How many rows went.
pub(crate) fn drop_history_naming(
    tx: &Transaction<'_>,
    forms: &[&str],
) -> Result<usize, StoreError> {
    let mut gone = 0;
    for form in forms {
        gone += exec(
            tx,
            "forget the history's contacts of a person",
            "DELETE FROM address_book WHERE origin = 1 AND (contact = ?1 OR pn = ?1 OR lid = ?1)",
            params![form],
        )?;
    }
    Ok(gone)
}

/// The phone dropped the contact: its row goes and is returned (`None`: the mirror had none).
pub(crate) fn take(tx: &Transaction<'_>, contact: &str) -> Result<Option<Row>, StoreError> {
    let row = first(
        tx,
        "read an address-book contact",
        "SELECT contact, name, pn, lid FROM address_book WHERE contact = ?1",
        params![contact],
        row_of,
    )?;
    if row.is_some() {
        exec(
            tx,
            "forget an address-book contact",
            "DELETE FROM address_book WHERE contact = ?1",
            params![contact],
        )?;
    }
    Ok(row)
}

fn row_of(r: &kourier_db::rusqlite::Row<'_>) -> kourier_db::rusqlite::Result<Row> {
    Ok(Row {
        contact: r.get(0)?,
        name: r.get(1)?,
        pn: r.get(2)?,
        lid: r.get(3)?,
    })
}

/// Whether the mirror holds ANY row (of either origin) that is indexed under or names one of `forms`: the person is
/// already in the book, whatever the form the earlier entry used (W6a2 PRIV-4).
pub(crate) fn names_any(conn: &Connection, forms: &[&str]) -> Result<bool, StoreError> {
    for form in forms {
        let hit = first(
            conn,
            "ask whether an address-book person is known",
            "SELECT 1 FROM address_book WHERE contact = ?1 OR pn = ?1 OR lid = ?1 LIMIT 1",
            params![form],
            |r| r.get::<_, i64>(0),
        )?;
        if hit.is_some() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The address-book row of `person` (an encoded `pn:` or `lid:` id), found by the index id or by either form the row names.
/// Several rows for one person: the one with the smallest index id (a fixed choice, so a restate says the same).
pub(crate) fn of(conn: &Connection, person: &str) -> Result<Option<Row>, StoreError> {
    first(
        conn,
        "read the address-book row of a person",
        "SELECT contact, name, pn, lid FROM address_book
         WHERE contact = ?1 OR pn = ?1 OR lid = ?1 ORDER BY contact LIMIT 1",
        params![person],
        row_of,
    )
}

/// The core is told of `person` as a peer, member or author. `true` when he was not known before.
pub(crate) fn know(tx: &Transaction<'_>, person: &str) -> Result<bool, StoreError> {
    exec(
        tx,
        "record a person the core knows",
        "INSERT INTO persons_known(person) VALUES (?1) ON CONFLICT (person) DO NOTHING",
        params![person],
    )
    .map(|changed| changed > 0)
}

/// The core is NOT known to know `person` after all: a refused history row that made him known was dropped (fix F18). He is
/// made known again by the next zone that names him. `true` when a row went.
pub(crate) fn forget_known(tx: &Transaction<'_>, person: &str) -> Result<bool, StoreError> {
    exec(
        tx,
        "forget a person the core never got",
        "DELETE FROM persons_known WHERE person = ?1",
        params![person],
    )
    .map(|changed| changed > 0)
}

/// Which of the forms in `forms` the core knows (`persons_known`), in the order given.
pub(crate) fn known_of(conn: &Connection, forms: &[&str]) -> Result<Vec<String>, StoreError> {
    let mut known = Vec::new();
    for form in forms {
        let hit = rows(
            conn,
            "ask whether the core knows a person",
            "SELECT person FROM persons_known WHERE person = ?1",
            params![form],
            |r| r.get::<_, String>(0),
        )?;
        known.extend(hit);
    }
    Ok(known)
}

#[cfg(test)]
mod tests;
