//! The Threema `backend.db` (architecture §5.5, §7.4): opening it through `kourier-db` and its
//! migrations, and the zone of one operation with its kill points around the commit.

use std::path::Path;

use kourier_db::{DbKey, Migrations, Tx, ZoneDb, failpoint, migration};

use crate::error::{OpenError, StoreError};

/// The Threema schema component (`schema_version.component = 'threema'`). Its numbers are this
/// crate's own: each network has its own file. A committed migration is FROZEN: a change is a new
/// number. 1: the identity and the two nonce stores (TH1); 2: what a device join imports (TH4); 3:
/// the envelopes the receive path defers for a later build, with their kept sum (TH5); 4: what is being
/// sent, sealed (TH6a); 5: what the leader took from the chat server (TH6).
pub(crate) const MIGRATIONS: Migrations = Migrations::new(
    "threema",
    &[
        migration!(1, "0001_init"),
        migration!(2, "0002_join"),
        migration!(3, "0003_receive"),
        migration!(4, "0004_send"),
        migration!(5, "0005_leader"),
    ],
);

/// Opens (creating if needed) `backend.db` with its key and brings both schema components
/// (`core_outbox` from kourier-db, then `threema`) up to date. A wrong key fails before anything is
/// written. A file that carries any component other than those two (another network's backend,
/// the core, one that does not exist yet) is read first and refused as `OtherNetwork`, before any
/// migration: its bytes are not touched. THE ORDER IS THE RULE.
pub fn open(path: &Path, key: &DbKey) -> Result<ZoneDb, OpenError> {
    let mut conn = kourier_db::open(path, key)?;
    kourier_db::check_backend_file(&conn, &MIGRATIONS)?;
    kourier_db::migrate(&mut conn, &kourier_db::outbox::MIGRATIONS)?;
    kourier_db::migrate(&mut conn, &MIGRATIONS)?;
    Ok(ZoneDb::new(conn))
}

/// The kill points around the commit of one operation (`KOURIER_FAILPOINT=<name>[:<n>]`, names in
/// [`crate::failpoints`]): `inside` is hit with everything written and nothing committed, `after`
/// right after the commit.
#[derive(Clone, Copy)]
pub(crate) struct Points {
    pub(crate) inside: &'static str,
    pub(crate) after: &'static str,
}

/// ONE zone (one `BEGIN IMMEDIATE` transaction) around `f`: the kill point `inside` after `f`,
/// the commit, the kill point `after` (WhatsApp's `db::zone` on Kourier's `ZoneDb`). An error from
/// `f` rolls everything back; `Ok` comes only after the commit. An operation that writes more than
/// one row goes through this ONCE: two zones would be two commits (the kill loop finds that).
pub(crate) fn zone<T>(
    db: &ZoneDb,
    points: Points,
    f: impl FnOnce(&mut Tx<'_>) -> Result<T, StoreError>,
) -> Result<T, StoreError> {
    let out = db.zone(|tx| {
        let out = f(tx)?;
        failpoint::hit(points.inside);
        Ok::<T, StoreError>(out)
    })?;
    failpoint::hit(points.after);
    Ok(out)
}
