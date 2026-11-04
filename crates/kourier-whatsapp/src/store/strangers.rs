//! What a history chunk named of people the core does not know yet (W6a2 fix F11, PRIV-5): the tables `kept_pairs` and
//! `kept_names` of migration 8.
//!
//! They hold what the PHONE said, like `address_book`, not what the core was told: a `Rebuild` keeps them and they go with the
//! account's file. A row waits here until the person first becomes known to the core (`persons_known`), is stated once by
//! [`crate::strangers::release`] and is taken out ([`take_pairs`], [`take_name`]) in the same zone. Names are text a person
//! chose and numbers are what the phone vouched for: nothing here logs, and a row is only read and written by encoded ids and
//! the name.
//!
//! The functions run inside the caller's zone (`&Transaction`) or read (`&Connection`).

use kourier_db::rusqlite::{Transaction, params};
use wacore::store::error::StoreError;

use super::{exec, first, rows};

/// A number and a LID the chunk's mappings paired, kept for the day one of them is known. `true` when the pair is new.
pub(crate) fn keep_pair(tx: &Transaction<'_>, pn: &str, lid: &str) -> Result<bool, StoreError> {
    exec(
        tx,
        "keep a pair of a person the core does not know",
        "INSERT INTO kept_pairs(pn, lid) VALUES (?1, ?2) ON CONFLICT (pn, lid) DO NOTHING",
        params![pn, lid],
    )
    .map(|changed| changed > 0)
}

/// The push name the chunk gave `person`, kept for the day he is known. A person named again keeps the last name given.
pub(crate) fn keep_name(tx: &Transaction<'_>, person: &str, name: &str) -> Result<(), StoreError> {
    exec(
        tx,
        "keep the push name of a person the core does not know",
        "INSERT INTO kept_names(person, name) VALUES (?1, ?2)
         ON CONFLICT (person) DO UPDATE SET name = excluded.name",
        params![person, name],
    )
    .map(drop)
}

/// The kept pairs that name `person` (as the number or as the LID), as `(number, lid)` in order; they are taken out.
pub(crate) fn take_pairs(
    tx: &Transaction<'_>,
    person: &str,
) -> Result<Vec<(String, String)>, StoreError> {
    let pairs = rows(
        tx,
        "read the kept pairs of a person",
        "SELECT pn, lid FROM kept_pairs WHERE pn = ?1 OR lid = ?1 ORDER BY pn, lid",
        params![person],
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
    )?;
    if !pairs.is_empty() {
        exec(
            tx,
            "forget the kept pairs of a person",
            "DELETE FROM kept_pairs WHERE pn = ?1 OR lid = ?1",
            params![person],
        )?;
    }
    Ok(pairs)
}

/// The kept push name of `person`; it is taken out.
pub(crate) fn take_name(tx: &Transaction<'_>, person: &str) -> Result<Option<String>, StoreError> {
    let name = first(
        tx,
        "read the kept push name of a person",
        "SELECT name FROM kept_names WHERE person = ?1",
        params![person],
        |r| r.get::<_, String>(0),
    )?;
    if name.is_some() {
        exec(
            tx,
            "forget the kept push name of a person",
            "DELETE FROM kept_names WHERE person = ?1",
            params![person],
        )?;
    }
    Ok(name)
}
