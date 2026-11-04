//! The store's side of [`Mapping`] (W5a, chunk 7): the PN <-> LID pairs of `lid_pn_mapping` in backend.db that one
//! send needs, read in one go before the PURE `send::address` writes ids. A person the read did not cover is
//! written as it is (`address::in_namespace`'s fallback), never guessed.
//!
//! The current LID of a number is the newest row (`updated_at`, then the row's age): upstream's
//! `get_current_lid` and the store's own `get_pn_mapping` (`store/protocol.rs`). A LID has one number.

use std::collections::HashMap;

use kourier_db::rusqlite::Connection;
use kourier_model::UserId;
use wacore::store::error::StoreError;

use super::address::Mapping;
use crate::store::first;

/// The pairs one send reads.
#[derive(Default)]
pub(crate) struct Known {
    /// Number user -> its current LID user.
    lids: HashMap<String, String>,
    /// LID user -> its number user.
    pns: HashMap<String, String>,
}

impl Mapping for Known {
    fn lid_of(&self, pn: &str) -> Option<String> {
        self.lids.get(pn).cloned()
    }

    fn pn_of(&self, lid: &str) -> Option<String> {
        self.pns.get(lid).cloned()
    }
}

/// The current LID of the number `pn` (a user part): the newest row, as [`load`] reads it.
pub(crate) fn current_lid(conn: &Connection, pn: &str) -> Result<Option<String>, StoreError> {
    first(
        conn,
        "read the current LID of a number",
        "SELECT lid FROM lid_pn_mapping WHERE phone = ?1 \
         ORDER BY updated_at DESC, rowid DESC LIMIT 1",
        [pn],
        |r| r.get(0),
    )
}

/// The number of the LID `lid` (a user part), as [`load`] reads it.
pub(crate) fn number_of(conn: &Connection, lid: &str) -> Result<Option<String>, StoreError> {
    first(
        conn,
        "read the number of a LID",
        "SELECT phone FROM lid_pn_mapping WHERE lid = ?1",
        [lid],
        |r| r.get(0),
    )
}

/// The pairs of `users` (`pn:` and `lid:` ids; others are skipped) from the store.
pub(crate) fn load<'a>(
    conn: &Connection,
    users: impl IntoIterator<Item = &'a UserId>,
) -> Result<Known, StoreError> {
    let mut known = Known::default();
    for user in users {
        let value = user.value();
        match user.scheme() {
            "pn" if !known.lids.contains_key(value) => {
                if let Some(lid) = current_lid(conn, value)? {
                    known.lids.insert(value.to_owned(), lid);
                }
            }
            "lid" if !known.pns.contains_key(value) => {
                if let Some(pn) = number_of(conn, value)? {
                    known.pns.insert(value.to_owned(), pn);
                }
            }
            _ => {}
        }
    }
    Ok(known)
}

#[cfg(test)]
mod tests;
