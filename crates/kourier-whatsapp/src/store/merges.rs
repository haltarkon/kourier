//! The reads and the one write of the merge sweep (W6a2, chunk B9; decision D7 and the critic's H4 in
//! `target/wa/w6-decisions.md`): the pairs of a number and a LID that Kourier's store and the phone's address book hold, and
//! `merges_stated`, the pairs the core was told are one person.
//!
//! The rules are in [`crate::merges`]; this file only reads and writes the rows, by encoded ids (`pn:<digits>`,
//! `lid:<digits>`). A row of `lid_pn_mapping` has no prefix (upstream's store holds the bare user part), so the queries add
//! it. Nothing here logs; no id leaves it except through the return values.
//!
//! The functions run inside the caller's zone (`&Transaction`) or read (`&Connection`).

use kourier_db::rusqlite::{Connection, Transaction, params};
use wacore::store::error::StoreError;

use super::{exec, first, rows};

/// The current LID of one number and where the store learned it from, in encoded ids.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Current {
    pub(crate) pn: String,
    pub(crate) lid: String,
    /// Upstream's `learning_source` text (`usync`, `other`, ...).
    pub(crate) source: String,
}

impl std::fmt::Debug for Current {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Current(..)")
    }
}

/// The CURRENT LID of every number the store holds one for, newest `updated_at` first among a number's rows, then the
/// newest row (a number can have several LIDs; `get_pn_mapping` of upstream takes the same row), restricted to the numbers
/// whose pair is not stated yet. NOT scoped to the people the core knows: the scope (`persons_known`, D8's rule, a
/// stranger's number never reaches core.db) is applied by [`crate::merges::gather`] AFTER `decide`, to the chosen pairs, so
/// "the store and the book disagree" and "the book names two LIDs" still stop a merge when the other side is a stranger
/// (W6a2 PRIV-3). In number order.
pub(crate) fn currents(conn: &Connection) -> Result<Vec<Current>, StoreError> {
    rows(
        conn,
        "read the current lid of each number",
        "SELECT 'pn:' || m.phone, 'lid:' || m.lid, m.learning_source FROM lid_pn_mapping m
         WHERE m.rowid = (SELECT x.rowid FROM lid_pn_mapping x WHERE x.phone = m.phone
                          ORDER BY x.updated_at DESC, x.rowid DESC LIMIT 1)
           AND NOT EXISTS (SELECT 1 FROM merges_stated s WHERE s.pn = 'pn:' || m.phone AND s.lid = 'lid:' || m.lid)
         ORDER BY m.phone",
        [],
        |r| {
            Ok(Current {
                pn: r.get(0)?,
                lid: r.get(1)?,
                source: r.get(2)?,
            })
        },
    )
}

/// The pairs the phone's address book names (a `contact` mutation with both the number and the LID of the person: our
/// phone's own authenticated word) and not stated yet, of anybody (the scope is applied after `decide`, see [`currents`]).
/// In number order, each pair once.
pub(crate) fn book_pairs(conn: &Connection) -> Result<Vec<(String, String)>, StoreError> {
    rows(
        conn,
        "read the pairs of the address book",
        "SELECT DISTINCT b.pn, b.lid FROM address_book b
         WHERE b.pn IS NOT NULL AND b.lid IS NOT NULL
           AND NOT EXISTS (SELECT 1 FROM merges_stated s WHERE s.pn = b.pn AND s.lid = b.lid)
         ORDER BY b.pn, b.lid",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
}

/// The query of [`crosses_a_stated_pair`]: its two terms use the primary key (`pn`) and `merges_stated_lid` (fix F20, pinned by
/// the plan in the tests).
const CROSSES: &str =
    "SELECT 1 FROM merges_stated WHERE (pn = ?1 AND lid <> ?2) OR (lid = ?2 AND pn <> ?1) LIMIT 1";

/// Whether the core was told a pair that makes `pn` or `lid` part of ANOTHER pair: the number already has a LID or the LID
/// already has a number. A merge is final, so a second pair around one of the two ids would join two people (a recycled
/// number, a number change): it is never stated.
pub(crate) fn crosses_a_stated_pair(
    conn: &Connection,
    pn: &str,
    lid: &str,
) -> Result<bool, StoreError> {
    Ok(first(
        conn,
        "ask whether a pair crosses a stated one",
        CROSSES,
        params![pn, lid],
        |r| r.get::<_, i64>(0),
    )?
    .is_some())
}

/// Whether the core knows `pn` or `lid` as a person (`persons_known`).
pub(crate) fn either_known(conn: &Connection, pn: &str, lid: &str) -> Result<bool, StoreError> {
    Ok(first(
        conn,
        "ask whether the core knows either side of a pair",
        "SELECT 1 FROM persons_known WHERE person = ?1 OR person = ?2 LIMIT 1",
        params![pn, lid],
        |r| r.get::<_, i64>(0),
    )?
    .is_some())
}

/// The core is told `pn` and `lid` are one person. `true` when that is new (the caller states the merge); `false` when the
/// pair was stated before (nothing is written).
pub(crate) fn put_stated(tx: &Transaction<'_>, pn: &str, lid: &str) -> Result<bool, StoreError> {
    exec(
        tx,
        "record a merge the core was told",
        "INSERT INTO merges_stated(pn, lid) VALUES (?1, ?2) ON CONFLICT (pn, lid) DO NOTHING",
        params![pn, lid],
    )
    .map(|changed| changed > 0)
}

/// The newest `rowid` of `merges_stated` (0 for none), read by a zone that will [`claim_since`] the pairs it records. A pair that
/// was recorded before is not inserted again (`put_stated` does nothing), and every row a zone does insert takes a `rowid` above
/// the largest there was, so the rows above this mark are the pairs THIS zone recorded.
pub(crate) fn mark(tx: &Transaction<'_>) -> Result<i64, StoreError> {
    Ok(first(
        tx,
        "read the newest recorded merge",
        "SELECT COALESCE(MAX(rowid), 0) FROM merges_stated",
        [],
        |r| r.get::<_, i64>(0),
    )?
    .unwrap_or(0))
}

/// The pairs recorded since `mark` were recorded by the history row `seq` (G2): only that row's take-back forgets them.
pub(crate) fn claim_since(tx: &Transaction<'_>, mark: i64, seq: i64) -> Result<usize, StoreError> {
    exec(
        tx,
        "claim the merges a history row recorded",
        "UPDATE merges_stated SET by_seq = ?2 WHERE rowid > ?1",
        params![mark, seq],
    )
}

/// The core was NOT told `pn` and `lid` are one person after all (a refused history row that carried the pair was taken back,
/// fix F18): the record goes, so the redo, a later release or the sweep states the pair. ONLY when the history row `seq` recorded
/// the pair itself (G2, NEW-1): a pair an acked row, the live path or the sweep told before stays told, whatever the refused row
/// repeated. `true` when a row went.
pub(crate) fn forget_told_by(
    tx: &Transaction<'_>,
    pn: &str,
    lid: &str,
    seq: i64,
) -> Result<bool, StoreError> {
    exec(
        tx,
        "forget a merge the core never got",
        "DELETE FROM merges_stated WHERE pn = ?1 AND lid = ?2 AND by_seq = ?3",
        params![pn, lid, seq],
    )
    .map(|changed| changed > 0)
}

#[cfg(test)]
mod tests;
